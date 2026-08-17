import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useChatStore } from "../../stores/chatStore";
import { agentAvatarColor } from "../../lib/agentColors";
import { useIsDark, useUserPreferencesStore } from "../../stores/userPreferencesStore";

import { useAgentProfileModalStore } from "../../stores/agentProfileModalStore";
import { Bookmark, StickyNote, RefreshCw, Paperclip, MoreVertical, Puzzle, ListTodo, CheckCircle2, ClipboardList } from "lucide-react";
import { useCompetenciesModalStore } from "../../stores/competenciesModalStore";
import { useAssignmentEditorModalStore } from "../../stores/assignmentEditorModalStore";
import { CoordinatorBadge } from "../profile/CoordinatorBadge";

interface ChatHeaderProps {
  memoryOpen?: boolean;
  onToggleMemory?: () => void;
  bookmarkOpen?: boolean;
  onToggleBookmark?: () => void;
  assetsOpen?: boolean;
  onToggleAssets?: () => void;
  todosOpen?: boolean;
  onToggleTodos?: () => void;
  todosInProgressCount?: number;
  /** Status of the agent's active todo list, or null when none is live.
   *  "active" = running, "paused" = stopped. Drives the kebab indicator. */
  todoStatus?: "active" | "paused" | null;
  /** The active thread's focus path, forwarded into the Competencies modal
   *  on open so its "Project skills" section can scope to the right project. */
  focusPath?: string | null;
}

export function ChatHeader({ memoryOpen, onToggleMemory, bookmarkOpen, onToggleBookmark, assetsOpen, onToggleAssets, todosOpen, onToggleTodos, todosInProgressCount, todoStatus, focusPath }: ChatHeaderProps) {
  const profile = useChatStore((s) => s.selectedAgentProfile);
  const assignmentCount = useChatStore((s) => (profile ? s.assignmentsByAgent.get(profile.id)?.length ?? 0 : 0));
  const agents = useChatStore((s) => s.agents);
  const fetchAgents = useChatStore((s) => s.fetchAgents);
  const selectAgent = useChatStore((s) => s.selectAgent);
  const isDark = useIsDark();
  const circularAvatars = useUserPreferencesStore((s) => s.circularAvatars);
  const [refreshing, setRefreshing] = useState(false);
  const [menuOpen, setMenuOpen] = useState(false);
  const menuRef = useRef<HTMLDivElement>(null);

  // Coordinator level: read straight from the agent's snapshot, which the
  // server computes across the *full* delegation graph (see AgentSnapshot.
  // coordinator_level). Matches ChatSidebar's source of truth — recomputing
  // it client-side from `profile` alone would cap depth at 1, since we only
  // have the selected agent's own delegates_to, not its delegates' profiles.
  const coordLevel = useMemo(() => {
    if (!profile) return 0;
    return agents.find((a) => a.agent_id === profile.id)?.coordinator_level ?? 0;
  }, [profile, agents]);

  const invalidateCache = useChatStore((s) => s.invalidateCache);

  // Track previous todoStatus and profile to detect genuine live→done transitions.
  // prevTodoStatusRef starts as undefined (not null) so the initial mount is skipped.
  const prevTodoStatusRef = useRef<"active" | "paused" | null | undefined>(undefined);
  const prevProfileIdRef = useRef<string | undefined>(undefined);
  const [justCompleted, setJustCompleted] = useState(false);

  useEffect(() => {
    const prevTodoStatus = prevTodoStatusRef.current;
    const prevProfileId = prevProfileIdRef.current;
    prevTodoStatusRef.current = todoStatus;
    prevProfileIdRef.current = profile?.id;

    if (prevTodoStatus === undefined) return; // skip initial mount
    if (prevProfileId !== profile?.id) return; // skip agent switch
    if (prevTodoStatus !== null && todoStatus === null) {
      setJustCompleted(true);
      const timer = setTimeout(() => setJustCompleted(false), 2000);
      return () => clearTimeout(timer);
    }
  }, [todoStatus, profile?.id]);

  const handleRefresh = useCallback(async () => {
    if (!profile || refreshing) return;
    setRefreshing(true);
    try {
      await fetchAgents();
      invalidateCache(profile.id);
      await selectAgent(profile.id);
    } finally {
      setRefreshing(false);
    }
  }, [profile, refreshing, fetchAgents, invalidateCache, selectAgent]);

  // Close menu on outside click
  useEffect(() => {
    if (!menuOpen) return;
    const handler = (e: MouseEvent) => {
      if (menuRef.current && !menuRef.current.contains(e.target as Node)) {
        setMenuOpen(false);
      }
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [menuOpen]);

  if (!profile) return null;

  const snapshot = agents.find((a) => a.agent_id === profile.id);
  const hasActiveRun = snapshot?.has_active_run ?? false;

  return (
    <div className="flex items-center gap-[10px] px-[20px] py-[12px] border-b border-[var(--border-secondary)]">
      {/* Left side: avatar + name */}
      <div className="relative flex-shrink-0">
        <div
          className={`w-[40px] h-[40px] ${circularAvatars ? 'rounded-full' : 'rounded-[12px]'} flex items-center justify-center text-[20px] select-none cursor-pointer border-2 border-transparent hover:border-[var(--accent)]`}
          onClick={() => useAgentProfileModalStore.getState().openEdit(profile.id)}
          style={{ backgroundColor: agentAvatarColor(profile.name, isDark) }}
        >
          {profile.emoji ?? "🤖"}
        </div>
        {hasActiveRun && (
          <span className="absolute -bottom-[2px] -right-[2px] w-[15px] h-[15px] bg-[var(--presence-indicator,#22c55e)] rounded-full border-2 border-[var(--bg-secondary)]" />
        )}
      </div>

      {/* Name + description */}
      <div className="flex flex-col min-w-0 flex-1">
        <div className="flex items-center gap-[6px] min-w-0">
          <span className="text-[16px] font-bold text-[var(--text-primary)] leading-tight truncate">
            {profile.name}
          </span>
          {coordLevel > 0 && <CoordinatorBadge level={coordLevel} className="flex-shrink-0" />}
        </div>
        {profile.description && (
          <span className="text-[13px] text-[var(--text-secondary)] leading-tight truncate">
            {profile.description}
          </span>
        )}
      </div>

      {/* Action buttons */}
      <div className="flex items-center gap-[4px] flex-shrink-0">
        {/* Refresh */}
        <button
          onClick={handleRefresh}
          disabled={refreshing}
          className="w-[32px] h-[32px] rounded-[8px] flex items-center justify-center transition-colors cursor-pointer text-[var(--text-secondary)] hover:bg-[var(--bg-hover)] hover:text-[var(--text-primary)] disabled:opacity-50"
          title="Refresh"
        >
          <RefreshCw className={`w-[16px] h-[16px] ${refreshing ? "animate-spin" : ""}`} />
        </button>

        {/* Assignments — moved here from the (now threads-only) tab strip */}
        <button
          onClick={() => useAssignmentEditorModalStore.getState().openCreate(profile.id)}
          className="relative w-[32px] h-[32px] rounded-[8px] flex items-center justify-center transition-colors cursor-pointer text-[var(--text-secondary)] hover:bg-[var(--bg-hover)] hover:text-[var(--text-primary)]"
          title="Assignments"
        >
          <ClipboardList className="w-[16px] h-[16px]" />
          {assignmentCount > 0 && (
            <span className="absolute -top-[2px] -right-[2px] min-w-[14px] h-[14px] px-[3px] rounded-full bg-[var(--accent)] text-white text-[9px] font-bold flex items-center justify-center leading-none">
              {assignmentCount}
            </span>
          )}
        </button>

        {/* Competencies (skills + workflows) */}
        <button
          onClick={() => useCompetenciesModalStore.getState().open(profile.id, focusPath)}
          className="w-[32px] h-[32px] rounded-[8px] flex items-center justify-center transition-colors cursor-pointer text-[var(--text-secondary)] hover:bg-[var(--bg-hover)] hover:text-[var(--text-primary)]"
          title="Competencies"
        >
          <Puzzle className="w-[16px] h-[16px]" />
        </button>

        {/* More menu (bookmarks + memory + assets + todos) */}
        {(onToggleBookmark || onToggleMemory || onToggleAssets || onToggleTodos) && (
          <div className="relative" ref={menuRef}>
            <button
              onClick={() => setMenuOpen((o) => !o)}
              className={`w-[32px] h-[32px] rounded-[8px] flex items-center justify-center transition-all duration-150 cursor-pointer relative ${
                menuOpen || memoryOpen || bookmarkOpen || assetsOpen || todosOpen
                  ? "bg-[var(--text-primary)] text-[var(--bg-primary)]"
                  : justCompleted
                    ? "bg-[var(--accent)] text-white"
                    : todoStatus === "active"
                      ? "bg-[var(--accent)] text-white hover:opacity-90"
                      : todoStatus === "paused"
                        ? "bg-[var(--accent)] text-white opacity-60 hover:opacity-75"
                        : "text-[var(--text-secondary)] hover:bg-[var(--bg-hover)] hover:text-[var(--text-primary)]"
              }`}
              title={
                todoStatus === "active"
                  ? "Todo list running"
                  : todoStatus === "paused"
                    ? "Todo list paused"
                    : "More options"
              }
            >
              {/* Pulsing ring while a todo list is actively running. Only shown
                  in the button's resting (no-panel-open) state. */}
              {todoStatus === "active" &&
                !menuOpen && !memoryOpen && !bookmarkOpen && !assetsOpen && !todosOpen && (
                  <span
                    className="absolute inset-0 rounded-[8px] animate-ping pointer-events-none"
                    style={{ boxShadow: "0 0 0 2px color-mix(in srgb, var(--accent) 50%, transparent)" }}
                    aria-hidden
                  />
                )}
              {justCompleted && !menuOpen && !memoryOpen && !bookmarkOpen && !assetsOpen && !todosOpen
                ? <CheckCircle2 className="w-[16px] h-[16px]" />
                : <MoreVertical className="w-[16px] h-[16px]" />
              }
              {(todosInProgressCount ?? 0) > 0 && !todosOpen ? (
                <span className="absolute -top-[4px] -right-[4px] w-[14px] h-[14px] rounded-full bg-[var(--accent)] text-white flex items-center justify-center">
                  <ListTodo className="w-[9px] h-[9px]" />
                </span>
              ) : todoStatus && !todosOpen ? (
                <span
                  className={`absolute -top-[2px] -right-[2px] w-[8px] h-[8px] rounded-full border-2 border-[var(--bg-secondary)] bg-[var(--accent)] ${
                    todoStatus === "paused" ? "opacity-60" : ""
                  }`}
                />
              ) : null}
            </button>

            {menuOpen && (
              <div className="chat-more-menu absolute right-0 top-full mt-1 w-[180px] rounded-xl border border-[var(--border-primary)] bg-[var(--bg-secondary)] shadow-xl z-50 p-1 flex flex-col gap-[2px]">
                {onToggleBookmark && (
                  <button
                    onClick={() => { onToggleBookmark(); setMenuOpen(false); }}
                    className={`w-full flex items-center gap-2.5 px-3 py-2 rounded-lg text-left transition-colors cursor-pointer ${
                      bookmarkOpen
                        ? "bg-[var(--bg-hover)] text-[var(--text-primary)]"
                        : "text-[var(--text-primary)] hover:bg-[var(--bg-hover)]"
                    }`}
                  >
                    <Bookmark className="w-[15px] h-[15px] flex-shrink-0 text-[var(--text-secondary)]" />
                    <span className="text-[13px] font-medium text-[var(--text-primary)]">Bookmarks</span>
                  </button>
                )}
                {onToggleMemory && (
                  <button
                    onClick={() => { onToggleMemory(); setMenuOpen(false); }}
                    className={`w-full flex items-center gap-2.5 px-3 py-2 rounded-lg text-left transition-colors cursor-pointer ${
                      memoryOpen
                        ? "bg-[var(--bg-hover)] text-[var(--text-primary)]"
                        : "text-[var(--text-primary)] hover:bg-[var(--bg-hover)]"
                    }`}
                  >
                    <StickyNote className="w-[15px] h-[15px] flex-shrink-0 text-[var(--text-secondary)]" />
                    <span className="text-[13px] font-medium text-[var(--text-primary)]">Memories</span>
                  </button>
                )}
                {onToggleAssets && (
                  <button
                    onClick={() => { onToggleAssets(); setMenuOpen(false); }}
                    className={`w-full flex items-center gap-2.5 px-3 py-2 rounded-lg text-left transition-colors cursor-pointer ${
                      assetsOpen
                        ? "bg-[var(--bg-hover)] text-[var(--text-primary)]"
                        : "text-[var(--text-primary)] hover:bg-[var(--bg-hover)]"
                    }`}
                  >
                    <Paperclip className="w-[15px] h-[15px] flex-shrink-0 text-[var(--text-secondary)]" />
                    <span className="text-[13px] font-medium text-[var(--text-primary)]">Assets</span>
                  </button>
                )}
                {onToggleTodos && (
                  <button
                    onClick={() => { onToggleTodos(); setMenuOpen(false); }}
                    className={`w-full flex items-center gap-2.5 px-3 py-2 rounded-lg text-left transition-colors cursor-pointer ${
                      todosOpen
                        ? "bg-[var(--bg-hover)] text-[var(--text-primary)]"
                        : "text-[var(--text-primary)] hover:bg-[var(--bg-hover)]"
                    }`}
                  >
                    <div className="relative flex-shrink-0">
                      <ListTodo className="w-[15px] h-[15px] text-[var(--text-secondary)]" />
                      {(todosInProgressCount ?? 0) > 0 && (
                        <span className="absolute -top-[3px] -right-[3px] min-w-[12px] h-[12px] px-[2px] rounded-full bg-[var(--accent)] text-white text-[8px] font-bold flex items-center justify-center leading-none">
                          {todosInProgressCount}
                        </span>
                      )}
                    </div>
                    <span className="text-[13px] font-medium text-[var(--text-primary)]">Todos</span>
                  </button>
                )}
              </div>
            )}
          </div>
        )}
      </div>
    </div>
  );
}
