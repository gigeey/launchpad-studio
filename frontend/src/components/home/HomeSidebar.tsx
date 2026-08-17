import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { twMerge } from "tailwind-merge";
import { Search, Plus, Play, GitBranch, Hash, Loader2, ClipboardList, CalendarClock, User, ChevronRight, MoreVertical, X, Inbox } from "lucide-react";
import { agentHasAnyPendingForm, agentHasPendingSyncForm, useChatStore } from "../../stores/chatStore";
import { useNavigationStore } from "../../stores/navigationStore";
import { useAssignmentEditorModalStore } from "../../stores/assignmentEditorModalStore";
import { useAgentProfileModalStore } from "../../stores/agentProfileModalStore";
import { useReadStatusStore } from "../../stores/readStatusStore";
import { useIsDark, useUserPreferencesStore } from "../../stores/userPreferencesStore";
import { agentAvatarColor } from "../../lib/agentColors";
import { channel, subscribeChannel } from "../../lib/sseHub";
import { CHANNEL_KIND_LABELS, switchToThread } from "../../lib/threadNavigation";
import { resolveChannelThreadPartition, type ChannelOriginKind } from "../../lib/channelThreads";
import { groupHomeChannelThreads, type HomeChannelThreadInfo } from "../../lib/homeChannelGrouping";
import { resolveAssignmentThreadPartition } from "../../lib/assignmentThreads";
import { groupHomeAssignmentThreads, type HomeAssignmentThreadInfo } from "../../lib/homeAssignmentGrouping";
import { CHANNEL_KIND_ICON } from "../chat/ThreadTabStrip";
import { Tooltip } from "../ui/Tooltip";
import { CoordinatorBadge } from "../profile/CoordinatorBadge";
import { RenameThreadModal } from "../chat/RenameThreadModal";
import {
  isSyncQuestion,
  resolveAgentActivityMap,
  resolveThreadActivity,
  ThreadQuestionBadge,
  ThreadStreamingBadge,
  ThreadUnreadDot,
} from "../shared/ThreadActivityBadge";
import type { Thread } from "../../types/api";
import { ContentGate } from "../ContentGate";
import { SidebarListSkeleton } from "../shared/Skeletons";
import { useReadyLatch } from "../../hooks/useReadyLatch";
import { useAssignments } from "../../hooks/useAssignments";

const EMPTY_THREADS: Thread[] = [];

/** Home is a compact "jump to a recent conversation" surface, not a full
 *  replacement for the Chat tab — cap how many agents it lists so it stays
 *  scannable. Everything else is still one click away via the Chat rail. */
const MAX_RECENT_AGENTS = 12;

function isDefaultThread(thread: Thread): boolean {
  return thread.kind === "default";
}

/** Same fallback chain as ThreadsPanel/ThreadTabStrip use in Chat, so a
 *  thread never reads differently depending on which surface you found it
 *  from. */
function threadLabel(thread: Thread): string {
  if (isDefaultThread(thread)) return "Main";
  if (thread.title && thread.title.trim().length > 0) return thread.title;
  if (thread.auto_title && thread.auto_title.trim().length > 0) return thread.auto_title;
  // Channel-originated threads (rename modal placeholder, shared with the
  // Channels section below) fall back to the channel's own display name
  // instead of the generic "New thread" — same precedence
  // `channelThreadLabel` (lib/channelThreads.ts) uses for the row label
  // itself, so the modal's preview text never disagrees with what the row
  // already shows.
  if (thread.channel_origin) return CHANNEL_KIND_LABELS[thread.channel_origin.kind];
  return thread.kind === "branch" ? "Branch" : "New thread";
}

/** Main now shares the plain hash glyph with ordinary threads — the render
 *  site below boxes Main's icon in a small rounded square so it still reads
 *  as the distinct, always-there thread at a glance instead of blending into
 *  the rest of the list. */
function threadIcon(thread: Thread) {
  return thread.kind === "branch" ? GitBranch : Hash;
}

/** Main first, then the rest newest-created-first — the same ordering the
 *  Chat tab's thread strip/panel use, so a thread's position here never
 *  surprises someone used to either of those. Archived threads are dropped
 *  entirely — archiving (from the Chat tab's tab strip, or ThreadsPanel)
 *  hides a thread from every surface, Home included, until it's unarchived
 *  from ThreadsPanel's "Archived" section. */
function orderThreads(threads: Thread[]): Thread[] {
  const visible = threads.filter((t) => !t.archived_at);
  const main = visible.filter(isDefaultThread);
  const rest = visible.filter((t) => !isDefaultThread(t)).sort((a, b) => b.created_at.localeCompare(a.created_at));
  return [...main, ...rest];
}

function relativeTime(isoString: string | null): string {
  if (!isoString) return "";
  const date = new Date(isoString);
  const now = new Date();

  const isToday = date.toDateString() === now.toDateString();
  const yesterday = new Date(now);
  yesterday.setDate(now.getDate() - 1);
  const isYesterday = date.toDateString() === yesterday.toDateString();

  if (isToday) return date.toLocaleTimeString([], { hour: "numeric", minute: "2-digit", hour12: true });
  if (isYesterday) return "Yesterday";
  return date.toLocaleDateString([], { month: "numeric", day: "numeric", year: "2-digit" });
}

/** Slack-style compact home nav: a search box, a "Jobs" (automations/
 *  assignments) header with a stubbed add button, and the agents you've most
 *  recently chatted with — each expandable to reveal its threads one level
 *  in. Unlike ChatSidebar there's no message preview here on purpose; Home
 *  stays a quick jump-list, not a second inbox. */
export function HomeSidebar() {
  const navigate = useNavigate();
  // Home shares the ChatView route shape (`/home/:subMenuSlug`), so the
  // agent currently rendered in the main panel is just the slug — same
  // pattern ChatSidebar uses to know which of its own rows is "active".
  const { subMenuSlug } = useParams<{ subMenuSlug?: string }>();
  const [search, setSearch] = useState("");
  const [loadingThreadsFor, setLoadingThreadsFor] = useState<Set<string>>(new Set());
  // Section-level collapse, independent of the per-agent expand/collapse
  // below — toggled by clicking the "Jobs"/"Agents" header itself, same
  // disclosure-triangle convention as the per-agent rows below. Persisted
  // (same `user-preferences` localStorage-backed store that already
  // remembers the chat/task sidebar widths) so navigating away from Home
  // and back doesn't silently re-expand everything.
  const jobsCollapsed = useUserPreferencesStore((s) => s.homeJobsCollapsed);
  const setJobsCollapsed = useUserPreferencesStore((s) => s.setHomeJobsCollapsed);
  const agentsCollapsed = useUserPreferencesStore((s) => s.homeAgentsCollapsed);
  const setAgentsCollapsed = useUserPreferencesStore((s) => s.setHomeAgentsCollapsed);
  // Per-agent nested-thread expand/collapse — same persisted store, stored
  // as a plain array (Sets don't survive JSON.stringify) and read into a
  // Set here for O(1) `.has()` checks in the render below. This is what
  // makes an individual agent's "threads open" state (not just the Jobs/
  // Agents section collapse above) survive navigating away from Home and
  // back, same as the section-level flags.
  const homeExpandedAgentIds = useUserPreferencesStore((s) => s.homeExpandedAgentIds);
  const setHomeExpandedAgentIds = useUserPreferencesStore((s) => s.setHomeExpandedAgentIds);
  const expandedAgentIds = useMemo(() => new Set(homeExpandedAgentIds), [homeExpandedAgentIds]);
  // Ref mirrors the state below so a rapid double-click can't slip through
  // before the state update re-renders (same guard ChatView's own
  // "New thread" button uses via its `creatingThreadRef`) — the state copy
  // exists purely to drive the button's spinner.
  const creatingThreadForRef = useRef<Set<string>>(new Set());
  const [creatingThreadFor, setCreatingThreadFor] = useState<Set<string>>(new Set());
  // Shared in-flight guard for every `loadThreads(agentId)` call site below —
  // the SSE `thread_created` backfill AND the per-agent expand-to-load
  // effect both check/set this SAME ref before firing, so whichever of the
  // two claims an agent first is the only one that actually fetches it: no
  // agent is ever double-fetched just because both paths raced (e.g. a
  // channel message arrives for an agent the expand-effect already has in
  // flight). A plain ref, not state, since it's read/written synchronously
  // from render-adjacent code (an SSE callback, effect bodies) and never
  // itself drives a render. Cleared in each call site's own `.finally()`
  // once that fetch settles (success, failure, or abort) so a later,
  // genuinely new attempt isn't permanently locked out.
  const requestedThreadsRef = useRef<Set<string>>(new Set());

  const agents = useChatStore((s) => s.agents);
  const agentsLoading = useChatStore((s) => s.agentsLoading);
  const fetchAgents = useChatStore((s) => s.fetchAgents);
  const loadThreads = useChatStore((s) => s.loadThreads);
  const loadAllThreads = useChatStore((s) => s.loadAllThreads);
  const threadsHydrationError = useChatStore((s) => s.threadsHydrationError);
  const createFreshThread = useChatStore((s) => s.createFreshThread);
  const threadsByAgent = useChatStore((s) => s.threadsByAgent);
  const selectedThreadIdByAgent = useChatStore((s) => s.selectedThreadIdByAgent);
  const renameThread = useChatStore((s) => s.renameThread);
  const archiveThread = useChatStore((s) => s.archiveThread);
  // The thread currently targeted by the rename modal — opened via the
  // hover-revealed kebab button on a (non-default) thread row below, shared
  // by both the Agents section's own thread rows AND the Channels section's
  // rows further up (one modal instance, mounted once at the bottom of this
  // component). Holds the whole `Thread`, not just an id, same reasoning as
  // ThreadTabStrip's own `renameTarget`: the modal always has a title/
  // placeholder to pre-fill even if `threadsByAgent` changes underneath it
  // while it's open.
  const [renameTarget, setRenameTarget] = useState<Thread | null>(null);
  // Same live streaming/unread source ChatSidebar's `ThreadTabStrip` reads —
  // pulled here as raw store references (both are replaced wholesale on
  // every mutation, so plain `===` already skips no-op re-renders; no
  // `useShallow` needed) and reduced below into per-agent + per-thread
  // activity so Home can show "running now" / "something new" at a glance,
  // same as the Chat tab's tab strip does per-pill.
  const inFlightByAgent = useChatStore((s) => s.inFlightByAgent);
  const unreadThreadIds = useChatStore((s) => s.unreadThreadIds);
  const runningDelegatesByThread = useChatStore((s) => s.runningDelegatesByThread);
  // Sync-form source for the agent-row question rollup below — the async
  // counterpart (`pending_forms`) already lives on each `agent` snapshot, so
  // it needs no separate subscription.
  const pendingFormByAgent = useChatStore((s) => s.pendingFormByAgent);
  const isUnread = useReadStatusStore((s) => s.isUnread);
  const markRead = useReadStatusStore((s) => s.markRead);
  const setSelectedSubMenu = useNavigationStore((s) => s.setSelectedSubMenu);
  const isDark = useIsDark();
  const circularAvatars = useUserPreferencesStore((s) => s.circularAvatars);

  // Home "Channels" section — additive, separate from the agent-grouped list
  // above. Section-level
  // collapse + the group-by toggle + per-group expand all persist through
  // the same `user-preferences` store the rest of Home's disclosure state
  // above already uses.
  const channelsCollapsed = useUserPreferencesStore((s) => s.homeChannelsCollapsed);
  const setChannelsCollapsed = useUserPreferencesStore((s) => s.setHomeChannelsCollapsed);
  const channelsGroupBy = useUserPreferencesStore((s) => s.homeChannelsGroupBy);
  const setChannelsGroupBy = useUserPreferencesStore((s) => s.setHomeChannelsGroupBy);
  const homeExpandedChannelGroupKeys = useUserPreferencesStore((s) => s.homeExpandedChannelGroupKeys);
  const setHomeExpandedChannelGroupKeys = useUserPreferencesStore((s) => s.setHomeExpandedChannelGroupKeys);
  const expandedChannelGroupKeys = useMemo(
    () => new Set(homeExpandedChannelGroupKeys),
    [homeExpandedChannelGroupKeys],
  );

  // Home "Assignments" section — mirrors "Channels" immediately above: an
  // additive, cross-agent view over every assignment-originated conversation
  // Home currently has thread data for. Its own collapse state reuses
  // `jobsCollapsed`/`setJobsCollapsed` (it still lives under the existing
  // "Assignments" header above, not a second one) — only the group-by toggle
  // and per-group expand state are new, same persisted `user-preferences`
  // fields the Channels section's own toggle/expand use above.
  const assignmentsGroupBy = useUserPreferencesStore((s) => s.homeAssignmentsGroupBy);
  const setAssignmentsGroupBy = useUserPreferencesStore((s) => s.setHomeAssignmentsGroupBy);
  const homeExpandedAssignmentGroupKeys = useUserPreferencesStore((s) => s.homeExpandedAssignmentGroupKeys);
  const setHomeExpandedAssignmentGroupKeys = useUserPreferencesStore((s) => s.setHomeExpandedAssignmentGroupKeys);
  const expandedAssignmentGroupKeys = useMemo(
    () => new Set(homeExpandedAssignmentGroupKeys),
    [homeExpandedAssignmentGroupKeys],
  );
  // Cross-agent assignment definitions, for resolving an assignment id to its
  // display name below (`assignmentLookup`) — the same aggregate hook the
  // Assignments page itself uses, so a name shown here always agrees with
  // what that page calls the same assignment.
  const { assignments } = useAssignments();

  useEffect(() => {
    fetchAgents();
  }, [fetchAgents]);

  // Catch-all system-channel subscription (same pattern as ChatSidebar):
  // Home shows many agents at once, so no single per-agent SSE channel
  // covers all of them — this is what makes a thread created in the
  // background (e.g. by an inbound Slack message or a scheduled task) show
  // up here live instead of only after the user visits Chat and back.
  // Splices into `threadsByAgent` via the same `addThreadLive` action the
  // per-agent handler in useSSE.ts uses.
  //
  // An agent whose thread list Home hasn't fetched yet used to be skipped
  // outright on the assumption it'd "load fresh the first time it's
  // expanded" — true for a UI-initiated thread (the user is the one who'd
  // expand it), but false for a channel-originated thread (Telegram/
  // Discord/email): the user may never expand that agent, so the event
  // would be dropped and nothing would ever schedule a backfill, leaving
  // the thread permanently invisible. Instead, trigger a background
  // `loadThreads` for that agent so it gets hydrated regardless of whether
  // it's ever expanded. This is now largely a safety net rather than the
  // primary path — the bulk `loadAllThreads` effect further down already
  // covers every agent Home knows about on mount — but still matters for an
  // agent this event names that isn't in that bulk call's own agent list yet
  // (e.g. one that only just started existing). `requestedThreadsRef`
  // (shared with the expand-effect below) guards against a burst of events
  // for the same un-hydrated agent firing `loadThreads` more than once
  // concurrently, AND against duplicating a fetch the expand-effect already
  // has in flight; it's cleared once that fetch settles (success or
  // failure) so a later, genuinely new burst can still retry.
  useEffect(() => {
    const sub = subscribeChannel(channel.system(), {
      listeners: {
        thread_created(e) {
          let parsed: { agent_id?: unknown; payload?: { data?: { thread?: Thread } } };
          try {
            parsed = JSON.parse(e.data as string);
          } catch {
            return;
          }
          const agentId = typeof parsed?.agent_id === "string" ? parsed.agent_id : undefined;
          const thread = parsed?.payload?.data?.thread;
          if (!agentId || !thread?.id) return;
          if (!useChatStore.getState().threadsByAgent.has(agentId)) {
            if (!requestedThreadsRef.current.has(agentId)) {
              requestedThreadsRef.current.add(agentId);
              useChatStore
                .getState()
                .loadThreads(agentId)
                .catch((err) => {
                  console.error("[HomeSidebar] background loadThreads failed:", err);
                })
                .finally(() => {
                  requestedThreadsRef.current.delete(agentId);
                });
            }
            return;
          }
          useChatStore.getState().addThreadLive(agentId, thread);
        },
      },
    });
    return () => sub.close();
  }, []);

  // One activity flag per agent, aggregated across all of that agent's
  // threads — this is what lets a COLLAPSED agent row show "something is
  // running" / "something's new" without needing that agent's thread list
  // fetched yet (see `resolveAgentActivityMap`'s doc comment for why the
  // composite in-flight/unread keys alone are enough for that).
  const agentActivityMap = useMemo(
    () => resolveAgentActivityMap(inFlightByAgent, unreadThreadIds, runningDelegatesByThread),
    [inFlightByAgent, unreadThreadIds, runningDelegatesByThread],
  );

  // Chatted-with agents, most-recent-first — independent of the search box
  // below, since the Jobs button always targets the true most-recent agent
  // regardless of what's currently typed into search.
  const sortedAgents = useMemo(() => {
    return [...agents]
      .filter((a) => a.message_count > 0)
      .sort((a, b) => {
        const dateA = a.last_activity_at ? new Date(a.last_activity_at).getTime() : 0;
        const dateB = b.last_activity_at ? new Date(b.last_activity_at).getTime() : 0;
        return dateB - dateA;
      });
  }, [agents]);

  // Stable key for the bulk-hydration effect's dependency array below — NOT
  // `agents` itself. `agents` gets a brand-new array identity on every
  // `patchAgentSnapshot` call (`stores/chatStore.ts`), which fires from
  // `hooks/useSSE.ts` on every run-started/run-ended event for ANY agent,
  // not just the one whose status actually changed (it `.map()`s the whole
  // `agents` list regardless of which entry matched). Keying the effect on
  // that reference would re-fire the bulk fetch on every unrelated status
  // patch. The incident that produced this rule hit the older per-agent
  // pump that the bulk fetch replaced; the same reference-churn hazard
  // applies here, because `agents` still gets a new identity on every patch.
  //
  // Primitives compare by VALUE in a dependency array, so reducing the
  // effect's actual input (the set of agent ids it hydrates — nothing else
  // about those agents matters to it) down to a joined string means an
  // unrelated status-only patch — same ids, same order — produces the
  // identical string and the effect below correctly does NOT re-run. A
  // genuine change (an agent added/removed) still produces a different
  // string and still re-fires, which is exactly when it should.
  const knownAgentIdsKey = useMemo(() => agents.map((a) => a.agent_id).join(","), [agents]);

  // Eager thread hydration. Channels/Assignments below are pure `useMemo`s
  // over `threadsByAgent`, which historically filled in ONLY once an agent's
  // row was expanded — so on a cold Home visit both sections rendered a
  // confident "No … yet" that read identically to a genuinely empty result.
  // This proactively hydrates every agent Home currently knows about, on
  // mount, rather than waiting for that expansion.
  //
  // A single `loadAllThreads` call now covers every known agent at once —
  // a bulk `GET /threads` endpoint exists server-side, so there's no more
  // per-agent fan-out, no concurrency cap, and no in-flight bookkeeping
  // shared with the SSE backfill/expand-effect (those two still use
  // `loadThreads`/`requestedThreadsRef` for their own single-agent fetches,
  // entirely independent of this one bulk call — a race between "many
  // small requests competing for the same agent" simply can't happen
  // anymore for the bulk path, by construction, rather than by careful
  // dedup bookkeeping). `threadsHydrating` below is the section-level
  // loading affordance's only input (besides `agentsLoading`, see
  // `homeThreadsHydrating`) — it's true for exactly the lifetime of the one
  // in-flight bulk request.
  const [threadsHydrating, setThreadsHydrating] = useState(false);
  // Whichever bulk hydration attempt (mount/dependency-change below, or a
  // manual retry from the error state further down) is currently in flight
  // — a new attempt aborts the previous one outright rather than letting two
  // bulk fetches race, and unmount aborts whichever is currently active.
  // `loadAllThreads` itself never rejects (failures land in the store's
  // `threadsHydrationError` instead — see its doc comment in chatStore.ts),
  // so there's no `.catch` here; only the `finally` guard below, which
  // clears `threadsHydrating` only if this call is still the one driving it
  // (a superseded attempt — aborted above, by a newer run, or by unmount —
  // must not stomp a flag a later attempt already owns).
  const hydrationAbortControllerRef = useRef<AbortController | null>(null);
  const runBulkHydration = useCallback(
    (knownAgentIds: string[]) => {
      hydrationAbortControllerRef.current?.abort();
      const controller = new AbortController();
      hydrationAbortControllerRef.current = controller;
      setThreadsHydrating(true);
      loadAllThreads(knownAgentIds, controller.signal).finally(() => {
        if (hydrationAbortControllerRef.current === controller) {
          setThreadsHydrating(false);
        }
      });
    },
    [loadAllThreads],
  );
  useEffect(() => {
    const knownAgentIds = knownAgentIdsKey.split(",").filter(Boolean);
    if (knownAgentIds.length === 0) return;
    runBulkHydration(knownAgentIds);
    return () => {
      hydrationAbortControllerRef.current?.abort();
    };
  }, [knownAgentIdsKey, runBulkHydration]);

  // Manual retry affordance for the Channels/Assignments error states below
  // — re-fires the exact same bulk hydration the mount/dependency-change
  // effect above does, targeting the same known-agent roster.
  const handleRetryThreadHydration = useCallback(() => {
    const knownAgentIds = knownAgentIdsKey.split(",").filter(Boolean);
    if (knownAgentIds.length === 0) return;
    runBulkHydration(knownAgentIds);
  }, [knownAgentIdsKey, runBulkHydration]);

  const displayedAgents = useMemo(() => {
    if (!search) return sortedAgents.slice(0, MAX_RECENT_AGENTS);
    const query = search.toLowerCase();
    return agents
      .filter((a) => a.name.toLowerCase().includes(query))
      .sort((a, b) => {
        const dateA = a.last_activity_at ? new Date(a.last_activity_at).getTime() : 0;
        const dateB = b.last_activity_at ? new Date(b.last_activity_at).getTime() : 0;
        return dateB - dateA;
      });
  }, [agents, sortedAgents, search]);

  // Assignments are still agent-scoped server-side — until there's an
  // agent-agnostic creation flow, the Jobs "+" opens the modal against
  // whichever agent was most recently active. Wiring a real cross-agent
  // assignment flow is follow-up work.
  const mostRecentAgentId = sortedAgents[0]?.agent_id ?? null;

  // Lookup for the "Channels" section below — resolves an owning agent's
  // name/emoji for a channel thread without re-scanning `agents` per row.
  const agentsById = useMemo(() => new Map(agents.map((a) => [a.agent_id, a])), [agents]);

  // `id -> name` lookup `resolveAssignmentThreadPartition` resolves each
  // assignment thread's group label from — built once here from the
  // cross-agent `assignments` fetched above rather than per-agent, since an
  // assignment id is globally unique and Home needs to label a thread's
  // owning assignment regardless of which agent's thread list it came from.
  const assignmentLookup = useMemo(
    () => new Map(assignments.map((a) => [a.id, { name: a.name }])),
    [assignments],
  );

  // Every channel-originated thread this component currently has thread data
  // for, tagged with its owning agent. `resolveChannelThreadPartition` only
  // ever sees one agent's threads at a time (it's shared with ThreadTabStrip,
  // which is always scoped to a single agent) — merging across agents is
  // Home's own concern, since Home is the one surface showing several agents
  // at once. This memo itself only ever scans whatever's already in
  // `threadsByAgent` — it never fetches. Populating that map for every known
  // agent (not just an expanded one) is the bulk `loadAllThreads` effect's
  // job (above `displayedAgents`), so this section fills in as that settles
  // rather than this memo triggering its own fetches.
  const homeChannelThreadItems = useMemo<HomeChannelThreadInfo[]>(() => {
    const items: HomeChannelThreadInfo[] = [];
    for (const [agentId, threads] of threadsByAgent) {
      const agent = agentsById.get(agentId);
      if (!agent) continue;
      const partition = resolveChannelThreadPartition(agentId, threads, unreadThreadIds);
      for (const info of partition.channelThreads) {
        items.push({ ...info, agentId, agentName: agent.name, agentEmoji: agent.emoji ?? null });
      }
    }
    return items;
  }, [threadsByAgent, agentsById, unreadThreadIds]);

  const channelGroups = useMemo(
    () => groupHomeChannelThreads(homeChannelThreadItems, channelsGroupBy),
    [homeChannelThreadItems, channelsGroupBy],
  );

  const totalChannelUnread = useMemo(
    () => channelGroups.reduce((sum, group) => sum + group.unreadCount, 0),
    [channelGroups],
  );

  // Same cross-agent merge as `homeChannelThreadItems` above, just sourced
  // from `resolveAssignmentThreadPartition` (lib/assignmentThreads.ts)
  // instead — see that memo's doc comment for why this only ever scans
  // `threadsByAgent` rather than fetching anything itself.
  const homeAssignmentThreadItems = useMemo<HomeAssignmentThreadInfo[]>(() => {
    const items: HomeAssignmentThreadInfo[] = [];
    for (const [agentId, threads] of threadsByAgent) {
      const agent = agentsById.get(agentId);
      if (!agent) continue;
      const partition = resolveAssignmentThreadPartition(agentId, threads, unreadThreadIds, assignmentLookup);
      for (const info of partition.assignmentThreads) {
        items.push({ ...info, agentId, agentName: agent.name, agentEmoji: agent.emoji ?? null });
      }
    }
    return items;
  }, [threadsByAgent, agentsById, unreadThreadIds, assignmentLookup]);

  const assignmentGroups = useMemo(
    () => groupHomeAssignmentThreads(homeAssignmentThreadItems, assignmentsGroupBy),
    [homeAssignmentThreadItems, assignmentsGroupBy],
  );

  const totalAssignmentUnread = useMemo(
    () => assignmentGroups.reduce((sum, group) => sum + group.unreadCount, 0),
    [assignmentGroups],
  );

  // The thread currently shown in the main panel, resolved once here (not
  // just inside the Agents-section map below) so the Channels section can
  // also highlight its row when the open thread is a channel thread. Same
  // fallback the per-agent computation used: the explicitly selected
  // thread, else that agent's default thread.
  const activeAgentId = subMenuSlug ?? null;
  const activeThreadId = useMemo(() => {
    if (!activeAgentId) return undefined;
    const explicit = selectedThreadIdByAgent.get(activeAgentId);
    if (explicit) return explicit;
    const agentThreads = threadsByAgent.get(activeAgentId) ?? EMPTY_THREADS;
    return agentThreads.find(isDefaultThread)?.id;
  }, [activeAgentId, selectedThreadIdByAgent, threadsByAgent]);

  // Namespaced by mode ("channel:slack" vs "agent:<id>") so switching the
  // toggle can never accidentally expand/collapse the wrong kind of group —
  // a `ChannelOriginKind` and an agent id share the same string space.
  const channelGroupExpandKey = useCallback(
    (groupKey: string) => `${channelsGroupBy}:${groupKey}`,
    [channelsGroupBy],
  );

  const handleToggleChannelGroup = useCallback(
    (groupKey: string) => {
      const fullKey = channelGroupExpandKey(groupKey);
      const next = new Set(expandedChannelGroupKeys);
      if (next.has(fullKey)) next.delete(fullKey);
      else next.add(fullKey);
      setHomeExpandedChannelGroupKeys(Array.from(next));
    },
    [channelGroupExpandKey, expandedChannelGroupKeys, setHomeExpandedChannelGroupKeys],
  );

  // Namespaced the same way `channelGroupExpandKey` is above
  // ("assignment:<id>" vs "agent:<id>") — an assignment id and an agent id
  // share the same string space, so the mode prefix keeps the two grouping
  // modes' expand state from ever colliding.
  const assignmentGroupExpandKey = useCallback(
    (groupKey: string) => `${assignmentsGroupBy}:${groupKey}`,
    [assignmentsGroupBy],
  );

  const handleToggleAssignmentGroup = useCallback(
    (groupKey: string) => {
      const fullKey = assignmentGroupExpandKey(groupKey);
      const next = new Set(expandedAssignmentGroupKeys);
      if (next.has(fullKey)) next.delete(fullKey);
      else next.add(fullKey);
      setHomeExpandedAssignmentGroupKeys(Array.from(next));
    },
    [assignmentGroupExpandKey, expandedAssignmentGroupKeys, setHomeExpandedAssignmentGroupKeys],
  );

  const handleToggleAgent = useCallback(
    (agentId: string) => {
      const next = new Set(expandedAgentIds);
      if (next.has(agentId)) next.delete(agentId);
      else next.add(agentId);
      setHomeExpandedAgentIds(Array.from(next));
    },
    [expandedAgentIds, setHomeExpandedAgentIds],
  );

  // Loads threads for any agent that's already expanded but not yet fetched
  // — covers both a fresh toggle-open (handled below via `expandedAgentIds`
  // changing) and hydrating from a *persisted* expanded set on mount/
  // navigation-back, where `threadsByAgent` starts empty even though
  // `homeExpandedAgentIds` already lists agents as open. `requestedThreadsRef`
  // (hoisted near the top of the component, shared with the SSE backfill
  // above) dedupes in-flight requests across renders AND across that other
  // path, so re-running this effect (e.g. after every toggle) never
  // double-fetches an agent either already has in flight or complete.
  useEffect(() => {
    expandedAgentIds.forEach((agentId) => {
      if (useChatStore.getState().threadsByAgent.has(agentId)) return;
      if (requestedThreadsRef.current.has(agentId)) return;
      requestedThreadsRef.current.add(agentId);
      setLoadingThreadsFor((prev) => new Set(prev).add(agentId));
      loadThreads(agentId).finally(() => {
        requestedThreadsRef.current.delete(agentId);
        setLoadingThreadsFor((prev) => {
          const next = new Set(prev);
          next.delete(agentId);
          return next;
        });
      });
    });
  }, [expandedAgentIds, loadThreads]);

  // Reuses the exact same store actions Chat's own thread switching does
  // (see lib/threadNavigation.ts) — Home just points them at a different
  // entry route in, then hands off to ChatView's existing rendering.
  //
  // Deliberately navigates to /home/:agentId rather than /chat/:agentId —
  // the active top-level view (and therefore which sidebar AppShell mounts)
  // is derived from the URL's first path segment, so staying under /home
  // keeps HomeSidebar in place while only the main panel swaps to the
  // ChatView-rendered conversation. Chat's own "last selected agent" is left
  // untouched so the two tabs remember their own place independently.
  const handleSelectThread = useCallback(
    async (agentId: string, threadId: string) => {
      markRead(agentId);
      setSelectedSubMenu("home", agentId);
      await switchToThread(agentId, threadId);
      navigate(`/home/${agentId}`);
    },
    [markRead, navigate, setSelectedSubMenu],
  );

  // Same underlying store action (`createFreshThread`) and the same
  // post-creation flow as `handleSelectThread` above — a freshly created
  // thread should land in the main panel exactly like clicking an existing
  // one would, not skip the mark-read/navigate steps.
  const handleCreateThread = useCallback(
    async (agentId: string) => {
      if (creatingThreadForRef.current.has(agentId)) return;
      creatingThreadForRef.current.add(agentId);
      setCreatingThreadFor(new Set(creatingThreadForRef.current));
      try {
        const thread = await createFreshThread(agentId);
        markRead(agentId);
        setSelectedSubMenu("home", agentId);
        await switchToThread(agentId, thread.id);
        navigate(`/home/${agentId}`);
      } finally {
        creatingThreadForRef.current.delete(agentId);
        setCreatingThreadFor(new Set(creatingThreadForRef.current));
      }
    },
    [createFreshThread, markRead, navigate, setSelectedSubMenu],
  );

  // Close ("archive") a thread — same server-side action + persistence as
  // ThreadTabStrip's pill `X` (see chatStore's `archiveThread`): it does NOT
  // delete anything, just hides the thread from every surface until it's
  // unarchived from ThreadsPanel's "Archived" section. Unlike ChatView's own
  // `handleArchiveThread`, Home can be looking at a DIFFERENT agent than the
  // one whose thread just got closed (this row's agent needn't be the one
  // open in the main panel) — so the two ways of recovering from closing the
  // selected thread diverge:
  //  - If `agentId` IS the one currently rendered in Home's main panel,
  //    fall back exactly like ChatView does: `switchToThread` so the panel
  //    doesn't keep pointing at a thread that just vanished from every list.
  //  - Otherwise there's nothing on screen showing that thread anyway, so
  //    only the dangling `selectedThreadIdByAgent` pointer is fixed up, via
  //    the plain, non-fetching `selectThreadForAgent` — `switchToThread`
  //    would also flip the global `selectedAgentId` and fetch messages,
  //    silently stealing the main panel to an agent the operator isn't even
  //    looking at.
  // Reads `threadsByAgent`/`selectedThreadIdByAgent` fresh via `getState()`
  // rather than subscribing, so this stays a stable callback even though
  // it's invoked from deep inside the per-agent/per-thread render loop below.
  const handleArchiveThread = useCallback(
    (agentId: string, threadId: string) => {
      const state = useChatStore.getState();
      const threads = state.threadsByAgent.get(agentId) ?? EMPTY_THREADS;
      const defaultThreadId = threads.find(isDefaultThread)?.id;
      const currentSelected = state.selectedThreadIdByAgent.get(agentId) ?? defaultThreadId;
      const wasSelected = currentSelected === threadId;
      void archiveThread(threadId);
      if (wasSelected && defaultThreadId && defaultThreadId !== threadId) {
        if (subMenuSlug === agentId) {
          void switchToThread(agentId, defaultThreadId);
        } else {
          state.selectThreadForAgent(agentId, defaultThreadId);
        }
      }
    },
    [archiveThread, subMenuSlug],
  );

  const handleOpenJobs = useCallback(() => {
    if (!mostRecentAgentId) return;
    useAssignmentEditorModalStore.getState().openCreate(mostRecentAgentId);
  }, [mostRecentAgentId]);

  const ready = useReadyLatch(agents.length > 0, agentsLoading);

  // Minimum-honesty gate for the Channels/Assignments empty states below.
  // Both sections are pure derivations over `threadsByAgent` (see
  // `homeChannelThreadItems`/`homeAssignmentThreadItems` above), so an EMPTY
  // render is only trustworthy once every agent Home knows about has
  // actually been asked for its threads — while the agent list itself is
  // still loading, or the bulk hydration above is still in flight, "empty"
  // and "not verified yet" are indistinguishable without this flag.
  const homeThreadsHydrating = agentsLoading || threadsHydrating;

  return (
    <div className="flex flex-col flex-1 min-h-0">
      {/* Search input — filters the agent list below by name */}
      <div className="mx-[4px] mb-[8px] flex items-center gap-2">
        <div className="app-search-surface cursor-text border-[1px] border-[var(--search-border)] h-[32px] flex-1 flex items-center gap-1 px-[10px] rounded-[8px] bg-[var(--search-bg)] text-[var(--text-secondary)]">
          <Search className="w-[14px] h-[14px] text-[var(--text-secondary)] flex-shrink-0" />
          <input
            type="text"
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            placeholder="Find item..."
            className="flex-1 text-[15px] leading-[1.4667] bg-transparent outline-none text-[var(--sidebar-text-primary,var(--text-primary))] placeholder:text-[var(--text-secondary)]"
          />
        </div>
      </div>

      {/* Recent agents, each expandable to its own threads one level in.
          `overflow-x-hidden` alongside `overflow-y-auto` is required, not
          decorative — per the CSS overflow spec, setting only overflow-y to
          a non-visible value makes the browser treat overflow-x as "auto"
          too, so any 1px-ish content overflow silently draws a horizontal
          scrollbar at the bottom of the list. Same fix ProjectsSidebar
          already applies.

          Both the Assignments and Agents header rows live inside this same
          scrollable container (as two independent collapsible sections)
          rather than the Assignments header sitting above it — keeping them
          in one container is what makes their "+" buttons land at the exact
          same x position: this div's `pr-[5px]` (needed to counter the CSS
          overflow-x/overflow-y coupling above) shifts everything inside it
          left by 5px relative to anything rendered outside it, so a header
          placed outside would always read as misaligned with one placed
          inside. */}
      <div className="flex-1 overflow-y-auto overflow-x-hidden pr-[5px]">
        {/* Assignments header — toggles `jobsCollapsed`, which also hides the
            grouped run-thread body rendered below it (see the body block
            further down). The whole row is the click target (role="button"
            div, same pattern the per-agent rows below use) so hovering/
            clicking anywhere on the row — not just the label — toggles it,
            matching the Agents header below. */}
        <div
          role="button"
          tabIndex={0}
          onClick={() => setJobsCollapsed(!jobsCollapsed)}
          onKeyDown={(e) => {
            if (e.key === "Enter" || e.key === " ") {
              e.preventDefault();
              setJobsCollapsed(!jobsCollapsed);
            }
          }}
          aria-expanded={!jobsCollapsed}
          className="group mx-[4px] mb-[6px] px-[8px] py-[6px] rounded-[10px] flex items-center justify-between cursor-pointer transition-colors hover:bg-[var(--bg-hover)]"
        >
          <span className="flex items-center gap-[6px] text-[15px] font-bold text-[var(--sidebar-text-primary,var(--text-primary))]">
            <span className="relative w-[15px] h-[15px] flex-shrink-0">
              <ClipboardList className="w-[15px] h-[15px] absolute inset-0 transition-opacity group-hover:opacity-0" />
              <ChevronRight
                className={twMerge(
                  "w-[15px] h-[15px] absolute inset-0 opacity-0 transition-all group-hover:opacity-100",
                  !jobsCollapsed && "rotate-90",
                )}
              />
            </span>
            Assignments
            {totalAssignmentUnread > 0 && (
              <span className="flex-shrink-0">
                <ThreadUnreadDot id="home-assignments-section" />
              </span>
            )}
          </span>
          {/* Group-by toggle (same switch shape/behavior as the Channels
              section's own toggle below) plus the pre-existing "New
              assignment" button — both live in this one header row since,
              unlike Channels, Assignments has no separate sub-header of its
              own to host the toggle in. The group-by switch is always
              visible (persistent view setting), same reasoning as the
              Channels toggle. The "+" button is hover-gated AND collapses to
              zero width (not just opacity-0) when hidden — otherwise its
              reserved 22px + gap shifts the switch left of the Channels
              section's own toggle below, which has no such button to offset
              it. Width/margin animate alongside opacity so it slides into
              view on hover instead of popping in. */}
          <div className="flex items-center flex-shrink-0">
            <Tooltip
              label={
                assignmentsGroupBy === "agent"
                  ? "Grouped by agent — switch to by assignment"
                  : "Grouped by assignment — switch to by agent"
              }
              placement="top"
              className="flex items-center flex-shrink-0"
            >
              <button
                type="button"
                role="switch"
                aria-checked={assignmentsGroupBy === "agent"}
                aria-label="Group assignments by agent"
                data-testid="home-assignments-group-toggle"
                onClick={(e) => {
                  e.stopPropagation();
                  setAssignmentsGroupBy(assignmentsGroupBy === "agent" ? "assignment" : "agent");
                }}
                className={twMerge(
                  "relative w-[28px] h-[16px] rounded-full transition-colors cursor-pointer flex-shrink-0",
                  assignmentsGroupBy === "agent" ? "bg-[var(--accent)]" : "bg-[var(--border-primary)]",
                )}
              >
                <div
                  className={twMerge(
                    "absolute top-[2px] w-[12px] h-[12px] rounded-full bg-white shadow transition-transform",
                    assignmentsGroupBy === "agent" ? "translate-x-[14px]" : "translate-x-[2px]",
                  )}
                />
              </button>
            </Tooltip>
            <Tooltip
              label={mostRecentAgentId ? "New assignment" : "Chat with an agent first"}
              placement="top"
              className="ml-0 group-hover:ml-[4px] overflow-hidden transition-[margin]"
            >
              <button
                type="button"
                onClick={(e) => {
                  e.stopPropagation();
                  handleOpenJobs();
                }}
                disabled={!mostRecentAgentId}
                aria-label="New assignment"
                className="w-0 group-hover:w-[22px] h-[22px] flex items-center justify-center rounded-[6px] text-[var(--text-secondary)] opacity-0 group-hover:opacity-100 overflow-hidden hover:bg-[var(--bg-hover)] hover:text-[var(--sidebar-text-primary,var(--text-primary))] disabled:opacity-40 disabled:cursor-not-allowed transition-all cursor-pointer"
              >
                <Plus className="w-[16px] h-[16px] flex-shrink-0" />
              </button>
            </Tooltip>
          </div>
        </div>

        {/* Assignments section body — every assignment-originated
            conversation Home currently has thread data for, grouped either by
            assignment or by owning agent (`assignmentsGroupBy`). Same
            eager-hydration contract as the Channels section below: sourced
            purely from `threadsByAgent` (this section itself never fetches),
            which the bulk `loadAllThreads` effect above proactively fills in
            for every known agent on mount rather than waiting for that
            agent's row to be expanded — see that effect's doc comment. While
            it's still outstanding (`homeThreadsHydrating`), an empty result here
            renders a loading affordance instead of the "No … yet" text below,
            so a not-yet-verified list is never visually indistinguishable
            from a confirmed-empty one. */}
        {!jobsCollapsed && (
          <div className="flex flex-col mb-[2px]">
            {assignmentGroups.length === 0 ? (
              threadsHydrationError ? (
                // A failed bulk hydration is NOT the same thing as "this
                // agent genuinely has no assignment threads" — see
                // `threadsHydrationError`'s doc comment in chatStore.ts.
                // Takes priority over `homeThreadsHydrating` below: once a
                // fetch has failed, don't fall through to a stale "Loading…"
                // affordance for a request that has already settled.
                <div
                  data-testid="home-assignments-error"
                  className="flex flex-col items-center gap-1 text-[13px] text-[var(--text-secondary)] text-center py-2 px-[8px]"
                >
                  <span>Couldn't load assignments</span>
                  <button
                    type="button"
                    data-testid="home-assignments-retry"
                    onClick={handleRetryThreadHydration}
                    className="text-[var(--accent)] hover:underline cursor-pointer"
                  >
                    Retry
                  </button>
                </div>
              ) : homeThreadsHydrating ? (
                <div
                  data-testid="home-assignments-loading"
                  className="text-[13px] text-[var(--text-secondary)] text-center py-2 px-[8px]"
                >
                  Loading…
                </div>
              ) : (
                <div className="text-[13px] text-[var(--text-secondary)] text-center py-2 px-[8px]">
                  No assignment run threads yet
                </div>
              )
            ) : (
              assignmentGroups.map((group) => {
                const isAssignmentMode = assignmentsGroupBy === "assignment";
                const fullGroupKey = assignmentGroupExpandKey(group.key);
                const isGroupOpen = expandedAssignmentGroupKeys.has(fullGroupKey);
                // In "assignment" mode the group key is an assignment id —
                // resolved to its name via `assignmentLookup`, same
                // trim-then-fallback-to-a-truncated-id convention
                // `assignmentGroupLabel` (lib/assignmentThreads.ts) uses. In
                // "agent" mode every member thread already carries the same
                // `agentName`, so the first one stands in for the group,
                // same as the Channels section's own agent-mode label below.
                const groupLabel = isAssignmentMode
                  ? assignmentLookup.get(group.key)?.name.trim() || group.key.slice(0, 8)
                  : group.threads[0].agentName;

                return (
                  <div key={fullGroupKey} className="mb-[1px]">
                    <div
                      role="button"
                      tabIndex={0}
                      aria-expanded={isGroupOpen}
                      onClick={() => handleToggleAssignmentGroup(group.key)}
                      onKeyDown={(e) => {
                        if (e.key === "Enter" || e.key === " ") {
                          e.preventDefault();
                          handleToggleAssignmentGroup(group.key);
                        }
                      }}
                      className="group/assignment flex items-center gap-[6px] mx-[4px] px-[8px] py-[6px] rounded-[10px] cursor-pointer transition-colors hover:bg-[var(--bg-hover)]"
                    >
                      <div className="relative w-[22px] h-[22px] flex-shrink-0">
                        {isAssignmentMode ? (
                          <CalendarClock className="absolute inset-0 m-auto w-[13px] h-[13px] text-[var(--text-tertiary)] transition-opacity group-hover/assignment:opacity-0" />
                        ) : (
                          <div
                            className={twMerge(
                              "absolute inset-0 flex items-center justify-center text-[10px] transition-opacity duration-100 group-hover/assignment:opacity-0",
                              circularAvatars ? "rounded-full" : "rounded-[6px]",
                            )}
                            style={{ backgroundColor: agentAvatarColor(group.threads[0].agentName, isDark) }}
                          >
                            {group.threads[0].agentEmoji ?? "🤖"}
                          </div>
                        )}
                        <ChevronRight
                          className={twMerge(
                            "absolute inset-0 m-auto w-[13px] h-[13px] text-[var(--text-tertiary)] opacity-0 transition-all group-hover/assignment:opacity-100",
                            isGroupOpen && "rotate-90",
                          )}
                        />
                      </div>
                      <span className="flex-1 min-w-0 truncate text-[15px] leading-[1.4667] font-medium text-[var(--sidebar-text-primary,var(--text-primary))] [font-variant-ligatures:common-ligatures]">
                        {groupLabel}
                      </span>
                      {group.unreadCount > 0 && (
                        <span className="flex-shrink-0">
                          <ThreadUnreadDot id={`home-assignment-group-${fullGroupKey}`} />
                        </span>
                      )}
                    </div>

                    {isGroupOpen && (
                      <div className="ml-[14px] pl-[6px] flex flex-col mt-[1px] mb-[3px]">
                        {group.threads.map((item) => {
                          // Same active-thread comparison the Agents and
                          // Channels sections use (`activeThreadId`, hoisted
                          // above and shared by all three).
                          const isActiveThread = activeThreadId === item.thread.id;
                          const ThreadIcon = threadIcon(item.thread);
                          return (
                            <div
                              key={item.thread.id}
                              data-testid={`home-assignment-thread-row-${item.thread.id}`}
                              className={twMerge(
                                "group/thread flex items-center rounded-[8px] transition-colors",
                                isActiveThread ? "bg-[var(--bg-hover)]" : "hover:bg-[var(--bg-hover)]",
                              )}
                            >
                              <button
                                type="button"
                                onClick={() => void handleSelectThread(item.agentId, item.thread.id)}
                                className="flex-1 min-w-0 flex items-center gap-[6px] px-[8px] py-[5px] text-left cursor-pointer"
                              >
                                <span className="flex items-center justify-center w-[16px] h-[16px] flex-shrink-0">
                                  {item.unread ? (
                                    <ThreadUnreadDot id={item.thread.id} />
                                  ) : (
                                    // Same Hash (or GitBranch, for a branch
                                    // thread) glyph the Agents section uses
                                    // for its own per-thread rows — via the
                                    // shared `threadIcon` helper — rather
                                    // than a second ClipboardList that'd just
                                    // repeat the group-row icon above.
                                    <ThreadIcon className="w-[12px] h-[12px] text-[var(--text-tertiary)]" />
                                  )}
                                </span>
                                <span className="truncate text-[15px] leading-[1.4667] text-[var(--sidebar-text-primary,var(--text-primary))] [font-variant-ligatures:common-ligatures]">
                                  {item.label}
                                </span>
                              </button>
                            </div>
                          );
                        })}
                      </div>
                    )}
                  </div>
                );
              })
            )}
          </div>
        )}

        {/* "Channels" section — additive unified inbox for every
            channel-originated conversation Home currently has thread data
            for. Sits
            alongside, not inside, the Agents list below: a channel
            conversation never appears twice, but this section exists
            precisely so it doesn't have to be found by expanding its owning
            agent first. Same whole-row-clickable header pattern as Jobs/
            Agents. Fed entirely from `threadsByAgent` (this section itself
            never fetches) — the bulk `loadAllThreads` effect above
            proactively fills that in for every known agent on mount, with a loading
            affordance covering the empty case below while it's still
            outstanding (`homeThreadsHydrating`), same contract the
            Assignments section above documents. */}
        <div
          role="button"
          tabIndex={0}
          onClick={() => setChannelsCollapsed(!channelsCollapsed)}
          onKeyDown={(e) => {
            if (e.key === "Enter" || e.key === " ") {
              e.preventDefault();
              setChannelsCollapsed(!channelsCollapsed);
            }
          }}
          aria-expanded={!channelsCollapsed}
          className="group mx-[4px] mb-[2px] px-[8px] py-[6px] rounded-[10px] flex items-center justify-between cursor-pointer transition-colors hover:bg-[var(--bg-hover)]"
        >
          <span className="flex items-center gap-[6px] text-[15px] font-bold text-[var(--sidebar-text-primary,var(--text-primary))]">
            <span className="relative w-[15px] h-[15px] flex-shrink-0">
              <Inbox className="w-[15px] h-[15px] absolute inset-0 transition-opacity group-hover:opacity-0" />
              <ChevronRight
                className={twMerge(
                  "w-[15px] h-[15px] absolute inset-0 opacity-0 transition-all group-hover:opacity-100",
                  !channelsCollapsed && "rotate-90",
                )}
              />
            </span>
            Channels
            {totalChannelUnread > 0 && (
              <span className="flex-shrink-0">
                <ThreadUnreadDot id="home-channels-section" />
              </span>
            )}
          </span>
          {/* Group-by toggle — same switch shape/behavior as
              AssignmentList's "Group by agent" toggle (scheduledListGroupByAgent),
              just driving a two-value store field instead of a boolean since
              "by channel" (the default) needs its own label too.
              Always visible (not hover-gated like the "+" buttons above) since
              it's a persistent view setting, not an occasional action. */}
          <Tooltip
            label={channelsGroupBy === "agent" ? "Grouped by agent — switch to by channel" : "Grouped by channel — switch to by agent"}
            placement="top"
            className="flex items-center flex-shrink-0"
          >
            <button
              type="button"
              role="switch"
              aria-checked={channelsGroupBy === "agent"}
              aria-label="Group channels by agent"
              data-testid="home-channels-group-toggle"
              onClick={(e) => {
                e.stopPropagation();
                setChannelsGroupBy(channelsGroupBy === "agent" ? "channel" : "agent");
              }}
              className={twMerge(
                "relative w-[28px] h-[16px] rounded-full transition-colors cursor-pointer flex-shrink-0",
                channelsGroupBy === "agent" ? "bg-[var(--accent)]" : "bg-[var(--border-primary)]",
              )}
            >
              <div
                className={twMerge(
                  "absolute top-[2px] w-[12px] h-[12px] rounded-full bg-white shadow transition-transform",
                  channelsGroupBy === "agent" ? "translate-x-[14px]" : "translate-x-[2px]",
                )}
              />
            </button>
          </Tooltip>
        </div>

        {!channelsCollapsed && (
          <div className="flex flex-col mb-[2px]">
            {channelGroups.length === 0 ? (
              threadsHydrationError ? (
                // A failed bulk hydration is NOT the same thing as "this
                // agent genuinely has no channel threads" — see
                // `threadsHydrationError`'s doc comment in chatStore.ts.
                // Takes priority over `homeThreadsHydrating` below: once a
                // fetch has failed, don't fall through to a stale "Loading…"
                // affordance for a request that has already settled.
                <div
                  data-testid="home-channels-error"
                  className="flex flex-col items-center gap-1 text-[13px] text-[var(--text-secondary)] text-center py-2 px-[8px]"
                >
                  <span>Couldn't load channels</span>
                  <button
                    type="button"
                    data-testid="home-channels-retry"
                    onClick={handleRetryThreadHydration}
                    className="text-[var(--accent)] hover:underline cursor-pointer"
                  >
                    Retry
                  </button>
                </div>
              ) : homeThreadsHydrating ? (
                <div
                  data-testid="home-channels-loading"
                  className="text-[13px] text-[var(--text-secondary)] text-center py-2 px-[8px]"
                >
                  Loading…
                </div>
              ) : (
                <div className="text-[13px] text-[var(--text-secondary)] text-center py-2 px-[8px]">
                  No channel conversations yet
                </div>
              )
            ) : (
              channelGroups.map((group) => {
                const isChannelMode = channelsGroupBy === "channel";
                const fullGroupKey = channelGroupExpandKey(group.key);
                const isGroupOpen = expandedChannelGroupKeys.has(fullGroupKey);
                const groupLabel = isChannelMode
                  ? CHANNEL_KIND_LABELS[group.key as ChannelOriginKind]
                  : group.threads[0].agentName;
                const GroupChannelIcon = isChannelMode ? CHANNEL_KIND_ICON[group.key as ChannelOriginKind] : null;

                return (
                  <div key={fullGroupKey} className="mb-[1px]">
                    <div
                      role="button"
                      tabIndex={0}
                      aria-expanded={isGroupOpen}
                      onClick={() => handleToggleChannelGroup(group.key)}
                      onKeyDown={(e) => {
                        if (e.key === "Enter" || e.key === " ") {
                          e.preventDefault();
                          handleToggleChannelGroup(group.key);
                        }
                      }}
                      className="group/channel flex items-center gap-[6px] mx-[4px] px-[8px] py-[6px] rounded-[10px] cursor-pointer transition-colors hover:bg-[var(--bg-hover)]"
                    >
                      <div className="relative w-[22px] h-[22px] flex-shrink-0">
                        {isChannelMode && GroupChannelIcon ? (
                          <GroupChannelIcon className="absolute inset-0 m-auto w-[13px] h-[13px] text-[var(--text-tertiary)] transition-opacity group-hover/channel:opacity-0" />
                        ) : (
                          <div
                            className={twMerge(
                              "absolute inset-0 flex items-center justify-center text-[10px] transition-opacity duration-100 group-hover/channel:opacity-0",
                              circularAvatars ? "rounded-full" : "rounded-[6px]",
                            )}
                            style={{ backgroundColor: agentAvatarColor(group.threads[0].agentName, isDark) }}
                          >
                            {group.threads[0].agentEmoji ?? "🤖"}
                          </div>
                        )}
                        <ChevronRight
                          className={twMerge(
                            "absolute inset-0 m-auto w-[13px] h-[13px] text-[var(--text-tertiary)] opacity-0 transition-all group-hover/channel:opacity-100",
                            isGroupOpen && "rotate-90",
                          )}
                        />
                      </div>
                      <span className="flex-1 min-w-0 truncate text-[15px] leading-[1.4667] font-medium text-[var(--sidebar-text-primary,var(--text-primary))] [font-variant-ligatures:common-ligatures]">
                        {groupLabel}
                      </span>
                      {group.unreadCount > 0 && (
                        <span className="flex-shrink-0">
                          <ThreadUnreadDot id={`home-channel-group-${fullGroupKey}`} />
                        </span>
                      )}
                    </div>

                    {isGroupOpen && (
                      <div className="ml-[14px] pl-[6px] flex flex-col mt-[1px] mb-[3px]">
                        {group.threads.map((item) => {
                          // Thread rows always use the Slack-style `#` glyph
                          // regardless of channel kind — only the group header
                          // above (`GroupChannelIcon`) shows the per-source icon.
                          const RowIcon = Hash;
                          // Same active-thread comparison the Agents section uses
                          // below (`activeThreadId`, hoisted above so both sections
                          // read the same value) — a selected channel thread now
                          // highlights here instead of nowhere.
                          const isActiveThread = activeThreadId === item.thread.id;
                          return (
                            // Same select-button/kebab split as the Agents-section
                            // thread rows below (`group/thread` wrapper, not a
                            // single <button>) — the hover-revealed rename kebab
                            // needs to be a sibling hit target, not nested inside
                            // the select button.
                            <div
                              key={item.thread.id}
                              data-testid={`home-channel-thread-row-${item.thread.id}`}
                              className={twMerge(
                                "group/thread flex items-center rounded-[8px] transition-colors",
                                isActiveThread ? "bg-[var(--bg-hover)]" : "hover:bg-[var(--bg-hover)]",
                              )}
                            >
                              <button
                                type="button"
                                onClick={() => void handleSelectThread(item.agentId, item.thread.id)}
                                className="flex-1 min-w-0 flex items-center gap-[6px] px-[8px] py-[5px] text-left cursor-pointer"
                              >
                                <span className="flex items-center justify-center w-[16px] h-[16px] flex-shrink-0">
                                  {item.unread ? (
                                    <ThreadUnreadDot id={item.thread.id} />
                                  ) : (
                                    <RowIcon className="w-[12px] h-[12px] text-[var(--text-tertiary)]" />
                                  )}
                                </span>
                                <span className="truncate text-[15px] leading-[1.4667] text-[var(--sidebar-text-primary,var(--text-primary))] [font-variant-ligatures:common-ligatures]">
                                  {item.label}
                                </span>
                              </button>
                              {/* Rename + archive entry points — the kebab
                                  reuses the same `renameTarget` state/modal
                                  instance the Agents-section thread rows
                                  below already render, so there's exactly one
                                  rename modal mounted regardless of which
                                  section opened it. Archive reuses the exact
                                  same `handleArchiveThread` the Agents-section
                                  rows call (it's already generic over
                                  agentId/threadId, nothing channel-specific
                                  needed) — same non-destructive "hide, don't
                                  delete" semantics as an ordinary thread's
                                  close button. Once archived,
                                  `resolveChannelThreadPartition` drops the
                                  thread from `channelGroups` entirely (see
                                  that function's own doc comment in
                                  lib/channelThreads.ts), so the row
                                  disappears from this section on the next
                                  render — there is no "Archived channels"
                                  recovery view yet, unlike ordinary threads'
                                  ThreadsPanel/overflow-panel "Archived"
                                  tab. */}
                              <div className="flex items-center gap-[2px] mr-[4px] flex-shrink-0 opacity-0 group-hover/thread:opacity-100 transition-opacity">
                                <button
                                  type="button"
                                  onClick={(e) => {
                                    e.stopPropagation();
                                    setRenameTarget(item.thread);
                                  }}
                                  aria-label="Rename thread"
                                  data-testid={`home-channel-thread-menu-${item.thread.id}`}
                                  className="w-[22px] h-[22px] flex items-center justify-center rounded-[6px] text-[var(--text-tertiary)] hover:bg-[var(--border-secondary)] hover:text-[var(--sidebar-text-primary,var(--text-primary))] transition-colors cursor-pointer"
                                >
                                  <MoreVertical className="w-[14px] h-[14px]" />
                                </button>
                                <button
                                  type="button"
                                  onClick={(e) => {
                                    e.stopPropagation();
                                    handleArchiveThread(item.agentId, item.thread.id);
                                  }}
                                  aria-label="Archive thread"
                                  data-testid={`home-channel-thread-archive-${item.thread.id}`}
                                  className="w-[22px] h-[22px] flex items-center justify-center rounded-[6px] text-[var(--text-tertiary)] hover:bg-[var(--border-secondary)] hover:text-[var(--sidebar-text-primary,var(--text-primary))] transition-colors cursor-pointer"
                                >
                                  <X className="w-[14px] h-[14px]" />
                                </button>
                              </div>
                            </div>
                          );
                        })}
                      </div>
                    )}
                  </div>
                );
              })
            )}
          </div>
        )}

        {/* Same whole-row-clickable pattern as the Jobs header above (and
            the per-agent rows below): a role="button" div, not a <button>,
            so the "New agent" action can live inside it as its own
            independent click target instead of illegally nesting buttons. */}
        <div
          role="button"
          tabIndex={0}
          onClick={() => setAgentsCollapsed(!agentsCollapsed)}
          onKeyDown={(e) => {
            if (e.key === "Enter" || e.key === " ") {
              e.preventDefault();
              setAgentsCollapsed(!agentsCollapsed);
            }
          }}
          aria-expanded={!agentsCollapsed}
          className="group mx-[4px] mb-[2px] px-[8px] py-[6px] rounded-[10px] flex items-center justify-between cursor-pointer transition-colors hover:bg-[var(--bg-hover)]"
        >
          <span className="flex items-center gap-[6px] text-[15px] font-bold text-[var(--sidebar-text-primary,var(--text-primary))]">
            <span className="relative w-[15px] h-[15px] flex-shrink-0">
              <User className="w-[15px] h-[15px] absolute inset-0 transition-opacity group-hover:opacity-0" />
              <ChevronRight
                className={twMerge(
                  "w-[15px] h-[15px] absolute inset-0 opacity-0 transition-all group-hover:opacity-100",
                  !agentsCollapsed && "rotate-90",
                )}
              />
            </span>
            Agents
          </span>
          {/* Only revealed on row hover, pinned to the far right — mirrors
              ChatSidebar's own "+" (same `openNew()` action) so creating an
              agent from Home behaves identically to creating one from Chat. */}
          <Tooltip label="New agent" placement="top">
            <button
              type="button"
              onClick={(e) => {
                e.stopPropagation();
                useAgentProfileModalStore.getState().openNew();
              }}
              aria-label="New agent"
              className="w-[22px] h-[22px] flex items-center justify-center rounded-[6px] text-[var(--text-secondary)] opacity-0 group-hover:opacity-100 hover:bg-[var(--bg-hover)] hover:text-[var(--sidebar-text-primary,var(--text-primary))] transition-colors cursor-pointer"
            >
              <Plus className="w-[16px] h-[16px]" />
            </button>
          </Tooltip>
        </div>

        {agentsCollapsed ? null : (
          <ContentGate ready={ready} skeleton={<SidebarListSkeleton rows={4} />} className="flex flex-col">
            {displayedAgents.length === 0 ? (
          <div className="text-[13px] text-[var(--text-secondary)] text-center py-4 px-[8px]">
            {search ? "No agents found" : "Chat with an agent to see it here"}
          </div>
        ) : (
          displayedAgents.map((agent) => {
            const isOpen = expandedAgentIds.has(agent.agent_id);
            // Channel-originated threads are excluded here — they render only
            // in the "Channels" section below (see `homeChannelThreadItems`).
            // Same shared partition ThreadTabStrip sources its own pills from
            // (`channelPartition.workingThreads`) so this
            // list and that one never disagree about what counts as a
            // "working" thread for a given agent.
            //
            // Assignment-originated threads are excluded the same way, for
            // the same reason — they render only in the "Assignments" section
            // above (see `homeAssignmentThreadItems`). Filtered inline rather
            // than via a second `resolveAssignmentThreadPartition` call since
            // only the exclusion is needed here, not its grouping.
            const threads = orderThreads(
              resolveChannelThreadPartition(
                agent.agent_id,
                threadsByAgent.get(agent.agent_id) ?? EMPTY_THREADS,
                unreadThreadIds,
              ).workingThreads.filter((t) => !t.assignment_origin),
            );
            const threadsLoading = loadingThreadsFor.has(agent.agent_id);

            // "Active" here means this agent's conversation is the one
            // currently rendered in the main panel (URL-driven, same as
            // ChatSidebar's `isSelected`) — not just that its thread list is
            // expanded. `activeThreadId` itself is hoisted above (shared
            // with the Channels section) so a selected channel thread is
            // still recognized correctly even though it's filtered out of
            // `threads` here.
            const isActiveAgent = subMenuSlug === agent.agent_id;
            // Live streaming/unread signal, same source ThreadTabStrip reads
            // (see `agentActivityMap` above) — streaming wins over unread,
            // and either one is suppressed while this agent is the one
            // already open in the main panel (its content is already
            // visible, so a badge next to its own name would just be noise).
            const agentActivity = agentActivityMap.get(agent.agent_id) ?? "none";
            // `agentActivity` only reflects SSE-populated maps, which stay
            // empty for an agent whose run started in the background (e.g.
            // via an Assignment or another agent's Delegate) and was never
            // selected this session — `has_active_run` is the connection-
            // independent REST-polled fallback ChatSidebar already reads for
            // the same reason.
            const streaming = !isActiveAgent && (agentActivity === "streaming" || agent.has_active_run);
            const unread =
              !isActiveAgent &&
              !streaming &&
              (agentActivity === "unread" || isUnread(agent.agent_id, agent.last_agent_activity_at ?? null));
            // Rollup: true when ANY of this agent's threads has an
            // unanswered form (sync `AskUserQuestionWithForm` or async
            // `pending_forms`), regardless of which thread that is or
            // whether this agent is the one currently open — unlike
            // `streaming`/`unread` above, this deliberately isn't suppressed
            // by `isActiveAgent`: the pending thread might not be the one
            // on screen, and an unanswered question stays actionable even
            // once you've opened the agent, unlike a passive activity notice.
            const hasPendingForm = agentHasAnyPendingForm(pendingFormByAgent, agent.agent_id, agent.pending_forms);
            // Sync-specific rollup — same "any thread" scope as `hasPendingForm`,
            // just narrowed to blocking `AskUserQuestionWithForm` calls, for the
            // badge's louder treatment below.
            const hasPendingSyncForm = agentHasPendingSyncForm(pendingFormByAgent, agent.agent_id);

            return (
              <div key={agent.agent_id} className="mb-[1px]">
                {/* Agent row — click toggles the thread list below; it does
                    NOT navigate on its own (see thread rows for that). */}
                <div
                  role="button"
                  tabIndex={0}
                  aria-expanded={isOpen}
                  onClick={() => handleToggleAgent(agent.agent_id)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter" || e.key === " ") {
                      e.preventDefault();
                      handleToggleAgent(agent.agent_id);
                    }
                  }}
                  className={twMerge(
                    "group flex items-center gap-[6px] mx-[4px] px-[8px] py-[6px] rounded-[10px] cursor-pointer transition-colors",
                    isActiveAgent ? "bg-[var(--sidebar-active-bg)]" : "hover:bg-[var(--bg-hover)]",
                  )}
                >
                  {/* Avatar/disclosure-triangle overlay: same 22x22 slot at
                      all times (no resizing) — the avatar fades out and a
                      solid triangle fades in on row hover, so the dropdown
                      affordance replaces the profile picture instead of
                      sitting beside it as a second icon. */}
                  <div className="relative w-[22px] h-[22px] flex-shrink-0">
                    <div
                      className={twMerge(
                        "absolute inset-0 flex items-center justify-center text-[12px] transition-opacity duration-100 group-hover:opacity-0",
                        circularAvatars ? "rounded-full" : "rounded-[6px]",
                      )}
                      style={{ backgroundColor: agentAvatarColor(agent.name, isDark) }}
                    >
                      {agent.emoji ?? "🤖"}
                    </div>
                    <Play
                      aria-hidden
                      fill="currentColor"
                      strokeWidth={0}
                      className={twMerge(
                        "absolute inset-0 m-auto w-[10px] h-[10px] text-[var(--text-secondary)] opacity-0 transition-opacity duration-100 group-hover:opacity-100",
                        isOpen && "rotate-90",
                      )}
                    />
                  </div>
                  <span className="flex-1 min-w-0 flex items-center gap-[5px]">
                    <span
                      className={twMerge(
                        "min-w-0 truncate text-[15px] leading-[1.4667] font-medium [font-variant-ligatures:common-ligatures]",
                        isActiveAgent ? "text-[var(--sidebar-active-text-primary)]" : "text-[var(--sidebar-text-primary,var(--text-primary))]",
                      )}
                    >
                      {agent.name}
                    </span>
                    {(agent.coordinator_level ?? 0) > 0 && (
                      <CoordinatorBadge level={agent.coordinator_level ?? 0} size={18} className="flex-shrink-0" />
                    )}
                  </span>
                  {hasPendingForm ? (
                    <span className="flex-shrink-0">
                      <ThreadQuestionBadge id={agent.agent_id} sync={hasPendingSyncForm} />
                    </span>
                  ) : streaming ? (
                    <span className="flex-shrink-0">
                      <ThreadStreamingBadge id={agent.agent_id} />
                    </span>
                  ) : unread ? (
                    <span className="flex-shrink-0">
                      <ThreadUnreadDot id={agent.agent_id} />
                    </span>
                  ) : null}
                  <span
                    className={twMerge(
                      "text-[11px] flex-shrink-0",
                      isActiveAgent ? "text-[var(--sidebar-active-text-secondary)]" : "text-[var(--text-tertiary)]",
                    )}
                  >
                    {relativeTime(agent.last_activity_at)}
                  </span>
                </div>

                {/* Indented thread list — only rendered while expanded. No
                    border-left guide (removed per design feedback); the
                    left margin/padding alone is enough to read as nested. */}
                {isOpen && (
                  <div className="ml-[14px] pl-[6px] flex flex-col mt-[1px] mb-[3px]">
                    {threadsLoading ? (
                      <div className="px-[8px] py-[4px] text-[12px] text-[var(--text-tertiary)]">Loading…</div>
                    ) : (
                      <>
                        {threads.length === 0 ? (
                          <div className="px-[8px] py-[4px] text-[12px] text-[var(--text-tertiary)]">No threads yet</div>
                        ) : (
                          threads.map((thread) => {
                            const ThreadIcon = threadIcon(thread);
                            const isActiveThread = activeThreadId === thread.id;
                            const isMainThread = isDefaultThread(thread);
                            // Per-thread streaming/unread/question, same source +
                            // priority rule as the agent-level aggregate above
                            // (and as ThreadTabStrip's own pills) — only surfaced
                            // for a thread that isn't the one already open, same
                            // reasoning as the agent-row suppression above.
                            const rawThreadActivity = resolveThreadActivity(
                              agent.agent_id,
                              thread,
                              inFlightByAgent,
                              unreadThreadIds,
                              runningDelegatesByThread,
                              pendingFormByAgent,
                              agent.pending_forms,
                            );
                            // `rawThreadActivity` only reflects SSE-populated
                            // maps, which stay empty for a thread whose run
                            // started in the background and was never opened
                            // this session — `running_thread_ids` is the
                            // connection-independent REST-polled fallback,
                            // same reasoning as the agent-row `has_active_run`
                            // check above. `question` is checked first and
                            // short-circuits the `running_thread_ids` fallback
                            // so a pending form still outranks streaming here,
                            // matching `resolveThreadActivity`'s own priority.
                            const threadActivity = isActiveThread
                              ? "none"
                              : rawThreadActivity === "question"
                                ? "question"
                                : rawThreadActivity === "streaming" || agent.running_thread_ids?.includes(thread.id)
                                  ? "streaming"
                                  : rawThreadActivity;
                            return (
                              // `group/thread` wrapper, not a single <button> —
                              // the hover-revealed kebab below needs to be a
                              // sibling hit target (a button can't nest inside
                              // a button), same split ThreadTabStrip's pills
                              // use for their select/close buttons. The
                              // active/hover background now lives on this
                              // wrapper so it still reads as one row.
                              <div
                                key={thread.id}
                                data-testid={`home-agent-thread-row-${thread.id}`}
                                className={twMerge(
                                  "group/thread flex items-center rounded-[8px] transition-colors",
                                  // Nested thread rows deliberately use the hover
                                  // token (not --sidebar-active-bg) when active —
                                  // that token is reserved for the top-level
                                  // active-agent row, so a selected thread here
                                  // shouldn't read as visually identical to it.
                                  isActiveThread ? "bg-[var(--bg-hover)]" : "hover:bg-[var(--bg-hover)]",
                                )}
                              >
                                <button
                                  type="button"
                                  onClick={() => void handleSelectThread(agent.agent_id, thread.id)}
                                  className="flex-1 min-w-0 flex items-center gap-[6px] px-[8px] py-[5px] text-left cursor-pointer"
                                >
                                  {/* Fixed 16x16 slot regardless of what's inside —
                                      icon, streaming badge, or unread dot all swap
                                      in place so toggling between them never
                                      shifts the label next to it. */}
                                  <span className="flex items-center justify-center w-[16px] h-[16px] flex-shrink-0">
                                    {threadActivity === "question" ? (
                                      <ThreadQuestionBadge
                                        id={thread.id}
                                        sync={isSyncQuestion(agent.agent_id, thread, pendingFormByAgent)}
                                      />
                                    ) : threadActivity === "streaming" ? (
                                      <ThreadStreamingBadge id={thread.id} />
                                    ) : threadActivity === "unread" ? (
                                      <ThreadUnreadDot id={thread.id} />
                                    ) : isMainThread ? (
                                      // Main's hash icon sits in a small rounded-square
                                      // outline — same glyph as a plain thread, boxed
                                      // so it still stands out as the one always-there
                                      // thread instead of looking like just another row.
                                      <span className="flex items-center justify-center w-[16px] h-[16px] rounded-[5px] border border-[var(--text-tertiary)] text-[var(--text-tertiary)]">
                                        <ThreadIcon className="w-[10px] h-[10px]" />
                                      </span>
                                    ) : (
                                      <ThreadIcon className="w-[12px] h-[12px] text-[var(--text-tertiary)]" />
                                    )}
                                  </span>
                                  {/* Active thread reads solely from its
                                      persistent --bg-hover background (see the
                                      wrapper's className above); the label keeps
                                      the same color/weight whether active or not,
                                      so there's no redundant accent-colored text. */}
                                  <span className="truncate text-[15px] leading-[1.4667] text-[var(--sidebar-text-primary,var(--text-primary))] [font-variant-ligatures:common-ligatures]">
                                    {threadLabel(thread)}
                                  </span>
                                </button>
                                {/* Rename + close entry points — Main can't
                                    be renamed or closed (it keeps its fixed
                                    label and always exists, same rule
                                    ThreadTabStrip's right-click/`X` enforce),
                                    so this pair is only rendered for the
                                    rest. Grouped in one wrapper so the
                                    opacity-0/group-hover reveal (same pattern
                                    the Jobs/Agents header "+" buttons above
                                    use) applies to both at once, keeping
                                    threads too far down the list to reach via
                                    the Chat tab's pill strip reachable here —
                                    both for renaming and for closing. */}
                                {!isMainThread && (
                                  <div className="flex items-center gap-[2px] mr-[4px] flex-shrink-0 opacity-0 group-hover/thread:opacity-100 transition-opacity">
                                    <button
                                      type="button"
                                      onClick={(e) => {
                                        e.stopPropagation();
                                        setRenameTarget(thread);
                                      }}
                                      aria-label="Rename thread"
                                      data-testid={`home-thread-menu-${thread.id}`}
                                      className="w-[22px] h-[22px] flex items-center justify-center rounded-[6px] text-[var(--text-tertiary)] hover:bg-[var(--border-secondary)] hover:text-[var(--sidebar-text-primary,var(--text-primary))] transition-colors cursor-pointer"
                                    >
                                      <MoreVertical className="w-[14px] h-[14px]" />
                                    </button>
                                    {/* Close = archive, same non-destructive
                                        semantics as ThreadTabStrip's pill
                                        `X` (see `handleArchiveThread` above)
                                        — the thread and its history stay
                                        intact, just hidden until unarchived
                                        from ThreadsPanel. */}
                                    <button
                                      type="button"
                                      onClick={(e) => {
                                        e.stopPropagation();
                                        handleArchiveThread(agent.agent_id, thread.id);
                                      }}
                                      aria-label="Archive thread"
                                      data-testid={`home-thread-archive-${thread.id}`}
                                      className="w-[22px] h-[22px] flex items-center justify-center rounded-[6px] text-[var(--text-tertiary)] hover:bg-[var(--border-secondary)] hover:text-[var(--sidebar-text-primary,var(--text-primary))] transition-colors cursor-pointer"
                                    >
                                      <X className="w-[14px] h-[14px]" />
                                    </button>
                                  </div>
                                )}
                              </div>
                            );
                          })
                        )}
                        {/* By-default row under the thread list: Main plus
                            this one fixed spot to spin up another thread —
                            same `createFreshThread` action + post-create
                            navigation ChatTab's tab-strip `+` uses, just
                            reached from Home instead. */}
                        <button
                          type="button"
                          onClick={() => void handleCreateThread(agent.agent_id)}
                          disabled={creatingThreadFor.has(agent.agent_id)}
                          className="flex items-center gap-[6px] px-[8px] py-[5px] rounded-[8px] text-left cursor-pointer transition-colors hover:bg-[var(--bg-hover)] disabled:opacity-50 disabled:cursor-not-allowed"
                        >
                          <span className="flex items-center justify-center w-[16px] h-[16px] flex-shrink-0">
                            {creatingThreadFor.has(agent.agent_id) ? (
                              <Loader2 className="w-[12px] h-[12px] animate-spin text-[var(--text-tertiary)]" />
                            ) : (
                              <Plus className="w-[12px] h-[12px] text-[var(--text-tertiary)]" />
                            )}
                          </span>
                          <span className="truncate text-[15px] leading-[1.4667] text-[var(--text-tertiary)] [font-variant-ligatures:common-ligatures]">
                            New thread
                          </span>
                        </button>
                      </>
                    )}
                  </div>
                )}
              </div>
            );
          })
        )}
          </ContentGate>
        )}
      </div>

      {/* Portaled to `document.body` (see RenameThreadModal) — safe to keep
          mounted here regardless of this sidebar's own scroll/overflow
          containers. Unmounting-on-close via `renameTarget &&` re-seeds a
          fresh `initialValue` from the current thread the next time it
          opens, same pattern ThreadTabStrip's own instance uses. */}
      {renameTarget && (
        <RenameThreadModal
          open
          initialValue={renameTarget.title ?? ""}
          placeholder={threadLabel(renameTarget)}
          onClose={() => setRenameTarget(null)}
          onSubmit={async (title) => { await renameThread(renameTarget.id, title); }}
        />
      )}
    </div>
  );
}
