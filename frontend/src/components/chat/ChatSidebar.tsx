import { useCallback, useEffect, useRef, useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { twMerge } from "tailwind-merge";
import { Search, Plus, ChevronRight, ListChecks } from "lucide-react";
import { motion, MotionConfig, LayoutGroup } from "framer-motion";
import { useChatStore } from "../../stores/chatStore";
import { useReadStatusStore } from "../../stores/readStatusStore";
import { useNavigationStore } from "../../stores/navigationStore";
import { useAgentProfileModalStore } from "../../stores/agentProfileModalStore";
import { agentAvatarColor } from "../../lib/agentColors";
import { useIsDark, useUserPreferencesStore } from "../../stores/userPreferencesStore";
import { channel, subscribeChannel } from "../../lib/sseHub";
import { switchToThread } from "../../lib/threadNavigation";
import { useWorkflowStore } from "../../stores/workflowStore";
import { TaskProgressRing } from "./TaskProgressRing";
import type { TaskSummary } from "../../types/workflow";
import { CoordinatorBadge } from "../profile/CoordinatorBadge";
import { ContentGate } from "../ContentGate";
import { SidebarListSkeleton } from "../shared/Skeletons";
import { useReadyLatch } from "../../hooks/useReadyLatch";

/** Format a date string as relative time (e.g., "2m ago", "1h ago"). */
function formatChatDate(isoString: string | null): string {
  if (!isoString) return "";
  const date = new Date(isoString);
  const now = new Date();

  const isToday = date.toDateString() === now.toDateString();
  const yesterday = new Date(now);
  yesterday.setDate(now.getDate() - 1);
  const isYesterday = date.toDateString() === yesterday.toDateString();

  if (isToday) {
    return date.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit', hour12: true });
  } else if (isYesterday) {
    return "Yesterday";
  } else {
    return date.toLocaleDateString([], { month: 'numeric', day: 'numeric', year: '2-digit' });
  }
}

const MIN_RING_SIZE = 64;
const MAX_RING_SIZE = 76;
const MAX_ITEM_GAP = 12;
const MIN_ITEM_GAP = 4;
const MAX_RECENT_TASKS = 4;
const RECENT_TASK_WINDOW_MS = 14 * 24 * 60 * 60 * 1000; // 2 weeks

function RecentTasksRow({ isDark, onNavigate }: { isDark: boolean; onNavigate: () => void }) {
  const navigate = useNavigate();
  const containerRef = useRef<HTMLDivElement>(null);
  const [ringSize, setRingSize] = useState(72);
  const [itemGap, setItemGap] = useState(MAX_ITEM_GAP);

  const tasks = useWorkflowStore((s) => s.tasks);
  const fetchTasks = useWorkflowStore((s) => s.fetchTasks);
  const startTask = useWorkflowStore((s) => s.startTask);

  // Fetch tasks on mount
  useEffect(() => {
    fetchTasks();
  }, [fetchTasks]);

  // Prioritize running tasks, then most recent. This ensures active work
  // gets the ring real estate instead of stale completed/pending tasks.
  // Only show tasks with activity in the past 2 weeks.
  const now = Date.now();
  const recentTasks: TaskSummary[] = [...tasks]
    .filter((t) => {
      const latest = t.completed_at ?? t.started_at ?? t.created;
      return now - new Date(latest).getTime() <= RECENT_TASK_WINDOW_MS;
    })
    .sort((a, b) => {
      const runningA = a.status === "running" ? 1 : 0;
      const runningB = b.status === "running" ? 1 : 0;
      if (runningA !== runningB) return runningB - runningA;
      return new Date(b.created).getTime() - new Date(a.created).getTime();
    })
    .slice(0, MAX_RECENT_TASKS);

  // Live progress sync for running tasks: one shared system-channel
  // subscription refetches on any workflow event, instead of a per-task
  // stream per running task.
  useEffect(() => {
    const refresh = () => fetchTasks();
    const sub = subscribeChannel(channel.system(), {
      listeners: {
        phase_started: refresh,
        phase_completed: refresh,
        phase_skipped: refresh,
        phase_paused: refresh,
        phase_failed: refresh,
        workflow_completed: refresh,
        workflow_task_started: refresh,
        workflow_task_failed: refresh,
      },
    });
    return () => sub.close();
  }, [fetchTasks]);

  const ITEM_LABEL_PAD = 24; // extra width per item for the label text

  const recalcSize = useCallback(() => {
    if (!containerRef.current) return;
    const containerWidth = containerRef.current.clientWidth;
    const itemCount = recentTasks.length || 1;

    // Strategy: shrink gap first, then shrink rings, then scroll.
    // 1. Try max ring + max gap
    const maxTotalNeeded = itemCount * (MAX_RING_SIZE + ITEM_LABEL_PAD) + (itemCount - 1) * MAX_ITEM_GAP;
    if (maxTotalNeeded <= containerWidth) {
      setRingSize(MAX_RING_SIZE);
      setItemGap(MAX_ITEM_GAP);
      return;
    }

    // 2. Keep max ring, shrink gap down to min
    const maxRingSpace = itemCount * (MAX_RING_SIZE + ITEM_LABEL_PAD);
    const remainingForGaps = containerWidth - maxRingSpace;
    const gapsCount = itemCount - 1;
    if (gapsCount > 0 && remainingForGaps >= gapsCount * MIN_ITEM_GAP) {
      setRingSize(MAX_RING_SIZE);
      setItemGap(Math.max(MIN_ITEM_GAP, Math.floor(remainingForGaps / gapsCount)));
      return;
    }

    // 3. Gap is at min, now shrink rings
    const availableForRings = containerWidth - gapsCount * MIN_ITEM_GAP;
    const perItem = availableForRings / itemCount - ITEM_LABEL_PAD;
    const clamped = Math.max(MIN_RING_SIZE, Math.min(MAX_RING_SIZE, Math.floor(perItem)));
    setRingSize(clamped);
    setItemGap(MIN_ITEM_GAP);
  }, [recentTasks.length]);

  useEffect(() => {
    recalcSize();
    const el = containerRef.current;
    if (!el) return;
    const ro = new ResizeObserver(recalcSize);
    ro.observe(el);
    return () => ro.disconnect();
  }, [recalcSize]);

  if (recentTasks.length === 0) return null;

  return (
    <div className="mx-[4px] mt-[8px] mb-[4px] bg-black/[0.03] dark:bg-white/[0.03] rounded-[16px] p-[12px] border border-[var(--border-secondary)]">
      {/* Header with chevron */}
      <div className="flex items-center justify-between mb-[4px] pl-[2px] pr-[2px]">
        <span className="text-[10px] font-semibold uppercase tracking-wider text-[var(--text-secondary)]">
          Recent Tasks
        </span>
        <button
          onClick={onNavigate}
          className="flex items-center gap-[2px] text-[10px] font-medium text-[var(--text-secondary)] hover:text-[var(--sidebar-text-primary,var(--text-primary))] transition-colors cursor-pointer"
        >
          <ChevronRight className="w-[14px] h-[14px]" />
        </button>
      </div>

      {/* Rings row — flex nowrap, scroll when items hit min size */}
      <div
        ref={containerRef}
        className="flex flex-nowrap items-start justify-center overflow-x-auto"
        style={{ gap: itemGap, scrollbarWidth: "none", msOverflowStyle: "none" } as React.CSSProperties}
      >
        {recentTasks.map((task) => (
          <div key={task.task_id} className="flex-shrink-0 flex justify-center" style={{ width: ringSize + 24 }}>
            <TaskProgressRing
              completedPhases={task.completed_phases}
              totalPhases={task.total_phases}
              workflowName={task.workflow}
              taskName={task.project_name}
              isDark={isDark}
              ringSize={ringSize}
              status={task.status}
              startTime={task.started_at ?? task.created}
              endTime={task.completed_at}
              isPaused={task.is_paused}
              onStart={() => startTask(task.task_id)}
              onClickName={() => navigate(`/tasks/${task.task_id}/detail`)}
            />
          </div>
        ))}
      </div>
    </div>
  );
}

export function ChatSidebar() {
  const navigate = useNavigate();
  const { subMenuSlug } = useParams<{ subMenuSlug?: string }>();
  const [search, setSearch] = useState("");

  const agents = useChatStore((s) => s.agents);
  const agentsLoading = useChatStore((s) => s.agentsLoading);
  const fetchAgents = useChatStore((s) => s.fetchAgents);
  const isUnread = useReadStatusStore((s) => s.isUnread);
  const setSelectedSubMenu = useNavigationStore((s) => s.setSelectedSubMenu);
  const markRead = useReadStatusStore((s) => s.markRead);
  const isDark = useIsDark();
  const circularAvatars = useUserPreferencesStore((s) => s.circularAvatars);
  const showRecentTasks = useUserPreferencesStore((s) => s.showRecentTasks);
  const fetchTasks = useWorkflowStore((s) => s.fetchTasks);

  useEffect(() => {
    fetchAgents();
  }, [fetchAgents]);

  // Keep sidebar metadata in sync for non-selected agents that are actively
  // processing: one shared system-channel subscription refetches on any
  // relevant event instead of opening a stream per tracked agent.
  useEffect(() => {
    const refreshSidebar = () => fetchAgents();
    const sub = subscribeChannel(channel.system(), {
      listeners: {
        run_started: refreshSidebar,
        message_received: refreshSidebar,
        text_complete: refreshSidebar,
        run_ended: refreshSidebar,
        "tasklist.created": refreshSidebar,
        "tasklist.completed": refreshSidebar,
        "tasklist.failed": refreshSidebar,
        "tasklist.status_changed": refreshSidebar,
      },
    });
    return () => sub.close();
  }, [fetchAgents]);

  // Global system channel: catches activity on agents the sidebar isn't
  // tracking yet (e.g. scheduled tasks firing on an idle agent). Refreshes
  // the agent list so unread indicators appear.
  useEffect(() => {
    const refresh = () => fetchAgents();
    // Workflow events: refresh task list so TaskProgressRing updates in real-time
    const refreshTasks = () => fetchTasks();
    const sub = subscribeChannel(channel.system(), {
      listeners: {
        system_message: refresh,
        run_started: refresh,
        message_received: refresh,
        text_complete: refresh,
        run_ended: refresh,
        "tasklist.created": refresh,
        "tasklist.status_changed": refresh,
        "tasklist.completed": refresh,
        "tasklist.failed": refresh,
        "agent.snapshot_updated": refresh,
        workflow_task_created: refreshTasks,
        workflow_task_started: refreshTasks,
        phase_completed: refreshTasks,
        phase_skipped: refreshTasks,
        workflow_completed: refreshTasks,
      },
    });
    return () => sub.close();
  }, [fetchAgents, fetchTasks]);

  // Re-fetch agents when the user starts searching (so newly created agents appear)
  const prevSearchRef = useRef("");
  useEffect(() => {
    if (search && !prevSearchRef.current) {
      fetchAgents();
    }
    prevSearchRef.current = search;
  }, [search, fetchAgents]);

  const filtered = (search
    ? agents.filter((a) => a.name.toLowerCase().includes(search.toLowerCase()))
    : agents.filter((a) => a.message_count > 0))
    .sort((a, b) => {
      const dateA = a.last_activity_at ? new Date(a.last_activity_at).getTime() : 0;
      const dateB = b.last_activity_at ? new Date(b.last_activity_at).getTime() : 0;
      return dateB - dateA;
    });
  const ready = useReadyLatch(agents.length > 0, agentsLoading);

  // A row's preview text (`agent.last_message`) is already thread-agnostic —
  // the backend stamps it from whichever thread most recently received a
  // message, regardless of which thread that was. But navigating used to
  // just open the agent at whatever thread happened to already be selected
  // (or the default), which could be a stale thread once an agent has more
  // than one. Resolve to the thread that actually holds the last message
  // (`thread_id` on the snapshot — `null` means the default thread, same
  // fallback `lib/threadNavigation.ts`'s `selectedThreadIdFor` uses) and jump
  // there explicitly. This only fires on a real click — no reactive effect
  // touches `selectedThreadIdByAgent` — so it can't clobber whatever thread
  // another view (e.g. Home) was last showing for this same agent.
  const handleSelectAgent = useCallback(
    async (agentId: string, threadId: string) => {
      markRead(agentId);
      setSelectedSubMenu("chat", agentId);
      await switchToThread(agentId, threadId);
      navigate(`/chat/${agentId}`);
    },
    [markRead, navigate, setSelectedSubMenu],
  );

  return (
    <div className="flex flex-col flex-1 min-h-0">
      {/* Search input */}
      <div className="mx-[4px] mb-[8px] flex items-center gap-2 relative z-20">
        <div className="app-search-surface cursor-text border-[1px] border-[var(--search-border)] h-[32px] flex-1 flex items-center gap-1 px-[10px] rounded-[8px] bg-[var(--search-bg)] text-[var(--text-secondary)]">
          <Search className="w-[14px] h-[14px] text-[var(--text-secondary)] flex-shrink-0" />
          <input
            type="text"
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            placeholder="Find chat..."
            className="flex-1 text-[15px] leading-[1.4667] bg-transparent outline-none text-[var(--sidebar-text-primary,var(--text-primary))] placeholder:text-[var(--text-secondary)]"
          />
        </div>
        <button
          onClick={() => useAgentProfileModalStore.getState().openNew()}
          className="flex-shrink-0 w-[32px] h-[32px] border-[1px] border-[var(--border-secondary)] rounded-[8px] bg-[var(--bg-secondary)] flex items-center justify-center text-[var(--text-secondary)] hover:bg-[var(--bg-hover)] hover:text-[var(--sidebar-text-primary,var(--text-primary))] transition-colors cursor-pointer"
        >
          <Plus className="w-[16px] h-[16px]" />
        </button>
      </div>

      {/*TODO: Agent list — if hundreds of agents cause render perf issues, virtualize with react-window or @tanstack/react-virtual */}
      <MotionConfig reducedMotion="never">
        <ContentGate
          ready={ready}
          skeleton={<SidebarListSkeleton />}
          className="flex-1 overflow-y-auto pr-[5px]"
        >
          <motion.div layoutScroll>
          {/* Recent tasks — scrolls with the chat list */}
          {showRecentTasks && <RecentTasksRow isDark={isDark} onNavigate={() => navigate("/tasks")} />}

          {/* Chats label */}
          <div className="mx-[4px] mb-[2px] px-[8px] py-[2px] text-[10px] font-bold uppercase tracking-wider text-[var(--text-secondary)]">
            Chats
          </div>

          {filtered.length === 0 ? (
            <div className="text-[13px] text-[var(--text-secondary)] text-center py-4">
              {search ? "No agents found" : "Search to find an agent and start chatting"}
            </div>
          ) : (
            <LayoutGroup id="chat-sidebar">
              {filtered.map((agent) => {
                const isSelected = subMenuSlug === agent.agent_id;
                const unread = !isSelected && isUnread(agent.agent_id, agent.last_agent_activity_at ?? null);
                return (
                  <motion.div
                    key={agent.agent_id}
                    initial={false}
                    // Rows re-sort on almost every SSE event (a message anywhere
                    // bumps last_activity_at). We deliberately don't animate that
                    // reorder (no `layout` prop) — a list that's constantly
                    // resettling from ambient activity reads as unstable, and it
                    // used to visibly keep sliding well after navigate() had
                    // already swapped the right panel on the same tick. Rows now
                    // just snap to their new position; only the initial reveal
                    // (via ContentGate, above) still animates.
                    className={twMerge(
                      "group relative p-[12px] flex items-start gap-3 cursor-pointer ml-[4px] mr-[1px] rounded-[12px]",
                      isSelected ? "bg-[var(--sidebar-active-bg)]" : "hover:bg-[var(--bg-hover)]"
                    )}
                    onClick={() => {
                      void handleSelectAgent(agent.agent_id, agent.thread_id ?? `default-${agent.agent_id}`);
                    }}
                  >
                    {/* Avatar/Emoji with optional tasklist ping ring */}
                    <div className="relative flex-shrink-0">
                      <div
                        className={`w-[40px] h-[40px] ${circularAvatars ? 'rounded-full' : 'rounded-[12px]'} flex items-center justify-center text-[20px]`}
                        style={{ backgroundColor: agentAvatarColor(agent.name, isDark) }}
                      >
                        {agent.emoji ?? "🤖"}
                      </div>
                      {agent.active_tasklist_title && !isSelected && (
                        <>
                          <span
                            className={`absolute inset-0 ${circularAvatars ? 'rounded-full' : 'rounded-[12px]'} animate-ping pointer-events-none`}
                            style={{
                              boxShadow: "0 0 0 3px color-mix(in srgb, var(--accent) 65%, transparent)",
                              backgroundColor: "color-mix(in srgb, var(--accent) 15%, transparent)",
                            }}
                            aria-hidden
                          />
                          <span className="absolute -bottom-[2px] -right-[2px] w-[14px] h-[14px] rounded-full bg-[var(--accent)] flex items-center justify-center border-2 border-[var(--bg-secondary)]">
                            <ListChecks className="w-[8px] h-[8px] text-white" />
                          </span>
                        </>
                      )}
                    </div>

                    {/* Content */}
                    <div className="flex-1 min-w-0 flex flex-col gap-0.5">
                      <div className="flex items-center justify-between gap-2">
                        <div className="flex items-center gap-[5px] min-w-0 flex-1">
                          <span className={twMerge("font-semibold text-[15px] truncate", isSelected ? "text-[var(--sidebar-active-text-primary)]" : "text-[var(--sidebar-text-primary,var(--text-primary))]")}>
                            {agent.name}
                          </span>
                          {(agent.coordinator_level ?? 0) > 0 && (
                            <CoordinatorBadge
                              level={agent.coordinator_level ?? 0}
                              className="flex-shrink-0"
                            />
                          )}
                        </div>
                        <span className={twMerge("text-[11px] flex-shrink-0", isSelected ? "text-[var(--sidebar-active-text-secondary)]" : "text-[var(--text-secondary)]")}>
                          {formatChatDate(agent.last_activity_at)}
                        </span>
                      </div>

                      <div className="flex items-start gap-2 min-h-[36px]">
                        {agent.has_active_run && !isSelected ? (
                          <div className="flex items-center gap-[4px] py-[2px] flex-1 min-w-0">
                            <span className={twMerge("text-[13px] italic", isSelected ? "text-[var(--sidebar-active-text-secondary)]" : "text-[var(--text-secondary)]")}>typing</span>
                            <div className="flex items-center gap-[2px] pt-[1px]">
                              {[0, 1, 2].map((i) => (
                                <motion.span
                                  key={i}
                                  className={twMerge("block w-[4px] h-[4px] rounded-full", isSelected ? "bg-[var(--sidebar-active-text-secondary)]" : "bg-[var(--text-secondary)]")}
                                  animate={{ y: [0, -3, 0] }}
                                  transition={{
                                    duration: 0.6,
                                    repeat: Infinity,
                                    delay: i * 0.2,
                                    ease: "easeInOut",
                                  }}
                                />
                              ))}
                            </div>
                          </div>
                        ) : agent.active_tasklist_title ? (
                          <div className="flex items-center gap-[4px] py-[2px] flex-1 min-w-0">
                            <ListChecks className={twMerge("w-[13px] h-[13px] flex-shrink-0", isSelected ? "text-[var(--sidebar-active-text-secondary)]" : "text-[var(--accent)]")} />
                            <span className={twMerge("text-[13px] truncate flex-1 min-w-0", isSelected ? "text-[var(--sidebar-active-text-primary)]" : "text-[var(--sidebar-text-primary,var(--text-primary))]")}>
                              {agent.active_tasklist_title}
                            </span>
                          </div>
                        ) : (
                          <div className={twMerge("text-[14px] line-clamp-2 leading-tight flex-1 min-w-0", isSelected ? "text-[var(--sidebar-active-text-secondary)]" : "text-[var(--text-secondary)]")}>
                            {agent.last_message || "No messages yet"}
                          </div>
                        )}
                        {unread && (
                          <span className="flex-shrink-0 mt-[5px] w-[10px] h-[10px] rounded-full bg-[var(--unread-badge-bg,var(--accent))]" />
                        )}
                      </div>
                    </div>

                    {/* Divider - absolute positioned at bottom, with left inset */}
                    {!isSelected && <div className="absolute bottom-0 left-[64px] right-[13px] border-b border-[var(--border-primary)] group-last:hidden" />}
                  </motion.div>
                );
              })}
            </LayoutGroup>
          )}
          </motion.div>
        </ContentGate>
      </MotionConfig>
    </div>
  );
}
