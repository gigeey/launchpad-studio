import { useState, useRef, useEffect, useCallback, type RefObject } from "react";
import { useNavigate, useLocation } from "react-router-dom";
import { twMerge } from "tailwind-merge";
import { ChevronLeft, ChevronRight, Plus, Brain, MoreHorizontal } from 'lucide-react';
import { SearchBar } from "../components/search/SearchBar";
import { SideBarIconClosed } from "../components/icons/SideBarIconClosed";
import logo from "../assets/LaunchpadLogo.png";
import { motion } from "framer-motion";
import { Tooltip } from "../components/ui/Tooltip";
import { viewConfigs, type ViewId, type ViewConfig } from "../config/navigation";

// Lucide has no purpose-built solid/outline dual set — `fill="currentColor"`
// tends to swallow any inner detail layered on top of an outer silhouette (a
// circle, a checkmark, a door, kanban bars, a folder's open-flap notch, a
// second person) into a same-color blob, since stroke and fill both resolve
// to currentColor. Confirmed by eye across every icon in the rail, including
// MessageSquare (Chat) — its speech-bubble tail reads as a blob once filled
// too. Skip the fill everywhere and use a bolder stroke instead — the
// active-state pill background already carries the "selected" signal.
const NO_FILL_ON_ACTIVE: ReadonlySet<ViewId> = new Set([
  "scheduled",
  "home",
  "assets",
  "tasks",
  "projects",
  "chat",
]);
import { useNavigationStore } from "../stores/navigationStore";
import { useUserPreferencesStore, resolveSidebarWidth } from "../stores/userPreferencesStore";
import { HomeSidebar } from "../components/home/HomeSidebar";
import { ChatSidebar } from "../components/chat/ChatSidebar";
import { ProjectsSidebar } from "../components/projects/ProjectsSidebar";
import { TasksSidebar } from "../components/workflow/TasksSidebar";
import { AssignmentsSidebar } from "../components/assignments/AssignmentsSidebar";
import { AssetsSidebar } from "../components/assets/AssetsSidebar";
import { useChatStore } from "../stores/chatStore";
import { useProjectStore } from "../stores/projectStore";
import { useWorkflowStore } from "../stores/workflowStore";
import { useAssignmentEditorModalStore } from "../stores/assignmentEditorModalStore";
import { SettingsPopover, type SettingsModalView, type SettingsPopoverAction } from "../components/SettingsPopover";
import { SettingsModal } from "../components/SettingsModal";
import { CreatePopover } from "../components/CreatePopover";
import { NavMorePopover } from "../components/NavMorePopover";
import { useNavRailOverflow } from "../hooks/useNavRailOverflow";
import { AgentProfileModal } from "../components/chat/AgentProfileModal";
import { useAgentProfileModalStore } from "../stores/agentProfileModalStore";
import { TaskCreateModal } from "../components/TaskCreateModal";
import { CompetenciesModal } from "../components/chat/CompetenciesModal";
import { AssignmentEditorModal } from "../components/assignments/AssignmentEditorModal";
import { CollectionsModal } from "../components/CollectionsModal";
import { useCollectionsModalStore } from "../stores/collectionsModalStore";
import { BannerStack } from "../components/BannerStack";
import { UpdateNotification } from "../components/UpdateNotification";
import { useBanners } from "../hooks/useBanners";
import { SSEManager } from "../components/SSEManager";
import { WorkspaceIndicator } from "../components/WorkspaceIndicator";
import { DeferredOutlet } from "../components/DeferredOutlet";
import * as api from "../lib/api";
import type { AgentProfile } from "../types/api";
import { openMemoriesWindow } from "../lib/windows";

// The subset of views whose sidebar gets kept mounted-but-hidden (instead of
// unmounted) once visited — see `visitedViews` in AppShell for why.
const SPECIAL_SIDEBAR_VIEWS: ReadonlyArray<ViewId> = ["home", "chat", "projects", "tasks", "scheduled", "assets"];

function BreadcrumbSubLabel({ viewId, slug, subMenuItems }: { viewId: ViewId; slug: string; subMenuItems: { id: string; label: string }[] }) {
  const agents = useChatStore((s) => s.agents);

  // Static sub-menu items (e.g. scheduled)
  const staticItem = subMenuItems.find((s) => s.id === slug);
  let label = staticItem?.label;

  // Dynamic: look up from store — "home" also resolves an agent name since
  // picking a thread there renders the same ChatView under /home/:agentId
  // (main panel changes, sidebar stays HomeSidebar).
  if (!label && (viewId === "chat" || viewId === "home")) {
    label = agents.find((a) => a.agent_id === slug)?.name;
  }

  if (!label) return null;

  return (
    <>
      <ChevronRight size={12} className="text-[var(--text-tertiary)] shrink-0" />
      <span className="text-[var(--text-primary)] truncate">{label}</span>
    </>
  );
}


export function AppShell() {
  const navigate = useNavigate();
  const location = useLocation();
  const sidebarOpen = useNavigationStore((s) => s.sidebarOpen);
  const toggleSidebar = useNavigationStore((s) => s.toggleSidebar);
  const setSelectedSubMenu = useNavigationStore((s) => s.setSelectedSubMenu);
  const sidebarWidths = useUserPreferencesStore((s) => s.sidebarWidths);
  const setSidebarWidthForView = useUserPreferencesStore((s) => s.setSidebarWidthForView);

  // Derive active view and sub-menu from URL. Computed early so the sidebar
  // width lookup below (each nav view remembers its own width) can key off
  // it — see handleMainMenuClick/handleSubMenuClick further down for the
  // other consumers of these same values.
  const pathParts = location.pathname.split("/").filter(Boolean);
  const activeViewPath = pathParts[0] ?? "chat";
  const activeSubMenuSlug = pathParts[1] ?? null;
  const activeViewConfig = viewConfigs.find((v) => v.path === `/${activeViewPath}`);
  const activeViewId: ViewId = activeViewConfig?.id ?? "chat";

  // Each nav view (Home, Chat, Tasks, ...) keeps its own sidebar width,
  // falling back to that view's default until the user resizes it.
  const sidebarWidth = resolveSidebarWidth(sidebarWidths, activeViewId);

  // Prefetch every sidebar's list data on mount so the *first* click on any
  // nav rail icon in a session shows cached data immediately instead of
  // running each tab's cold-load gate (blank -> skeleton -> content) the
  // first time it's visited. Revisits were already instant because each
  // list lives in a persisted store, not component state — this just warms
  // all of them up front, the same way agents already were, rather than
  // only on first visit to that tab.
  useEffect(() => {
    // Chained rather than fire-and-forget alongside the others below: the
    // sync-form rehydration reads `useChatStore.getState().agents`, so it
    // must run after this fetch's snapshot has landed, not just "eventually."
    // Runs exactly once per app load — `pendingFormByAgent` is still empty at
    // this point, so there's nothing live for it to clobber; see
    // `hydratePendingSyncFormsFromAgents`'s docstring for why later
    // `fetchAgents()` calls elsewhere must NOT repeat this.
    useChatStore
      .getState()
      .fetchAgents()
      .then(() => useChatStore.getState().hydratePendingSyncFormsFromAgents());
    useProjectStore.getState().fetchProjects();
    useWorkflowStore.getState().fetchWorkflows();
    useWorkflowStore.getState().fetchTasks();
  }, []);

  // Data prefetch (above) only warms each store's list — it doesn't stop the
  // *sidebar component itself* from fully unmounting and remounting every
  // time you leave a tab and come back, which was still a real source of
  // felt lag: remounting reruns every mount effect (agent/task refetches,
  // and — worst offender — ChatSidebar alone opens up to 3 fresh SSE
  // connections: the system stream, one per active-agent, and one per
  // running task) even though the underlying data was already cached.
  // Once a tab has been visited this session, keep its sidebar mounted for
  // good and just toggle visibility with `hidden` — switching back becomes a
  // pure CSS show/hide with zero remount cost. Tabs never opened this
  // session still never pay any of this (they're not in the set, so nothing
  // renders for them), so we don't reintroduce the cold-start-every-tab cost
  // Round 5 fixed.
  const [visitedViews, setVisitedViews] = useState<Set<ViewId>>(() => new Set([activeViewId]));
  useEffect(() => {
    setVisitedViews((prev) => (prev.has(activeViewId) ? prev : new Set(prev).add(activeViewId)));
  }, [activeViewId]);

  const [isResizing, setIsResizing] = useState(false);
  const sidebarRef = useRef<HTMLDivElement>(null);

  // Detect macOS fullscreen to adjust sidebar toggle position
  // (no traffic light buttons in fullscreen, so we can reclaim that space)
  // We wait for resize events to stop for 600ms before updating, so the
  // OS fullscreen animation completes before our slide animation starts.
  const [isFullscreen, setIsFullscreen] = useState(() =>
    !!document.fullscreenElement || window.innerHeight === screen.height
  );
  const fsTimerRef = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  useEffect(() => {
    const onResize = () => {
      clearTimeout(fsTimerRef.current);
      fsTimerRef.current = setTimeout(() => {
        const fs = !!document.fullscreenElement || window.innerHeight === screen.height;
        setIsFullscreen((prev) => {
          if (prev === fs) return prev;
          return fs;
        });
      }, 300);
    };
    window.addEventListener("resize", onResize);
    document.addEventListener("fullscreenchange", onResize);
    return () => {
      clearTimeout(fsTimerRef.current);
      window.removeEventListener("resize", onResize);
      document.removeEventListener("fullscreenchange", onResize);
    };
  }, []);

  // Settings popover + modal state
  const settingsIconRef = useRef<HTMLDivElement>(null);
  const [settingsPopoverOpen, setSettingsPopoverOpen] = useState(false);
  const [settingsModalView, setSettingsModalView] = useState<SettingsModalView | null>(null);

  // "+" create popover state — lets the user pick agent vs. scheduled item
  // before the relevant modal opens (both modals are store-driven and
  // already mounted below).
  const createIconRef = useRef<HTMLDivElement>(null);
  const [createPopoverOpen, setCreatePopoverOpen] = useState(false);

  // Nav rail overflow — Home is pinned and never collapses; the rest of
  // viewConfigs (in list order) fills in as many rows as vertically fit,
  // with the tail collapsing into a "More" row/popover. See
  // useNavRailOverflow's docstring for the measurement approach.
  const collapsibleNavItems = viewConfigs.filter((v) => v.id !== "settings" && v.id !== "home");
  const { containerRef: navRailContainerRef, pinnedRowRef: homeRowRef, visibleItems: visibleNavItems, overflowItems: overflowNavItems } =
    useNavRailOverflow(collapsibleNavItems, 12);
  const moreIconRef = useRef<HTMLDivElement>(null);
  const [moreOpen, setMoreOpen] = useState(false);

  // Collections modal — driven by store so other surfaces can trigger it later.
  const openCollectionsModal = useCollectionsModalStore((s) => s.open);

  // Banner system
  const openSettings = useCallback(() => setSettingsModalView("settings"), []);
  useBanners({ onOpenSettings: openSettings });

  // Agent profile modal — driven by store
  const agentModalMode = useAgentProfileModalStore((s) => s.mode);
  const closeAgentModal = useAgentProfileModalStore((s) => s.close);
  const isAgentModalOpen = agentModalMode !== null;
  const editAgentId = agentModalMode !== null && agentModalMode !== "new" ? agentModalMode : null;

  const createAgent = useChatStore((s) => s.createAgent);
  const updateAgent = useChatStore((s) => s.updateAgent);
  const cloneAgent = useChatStore((s) => s.cloneAgent);
  const openEditAgentModal = useAgentProfileModalStore((s) => s.openEdit);

  const [editProfile, setEditProfile] = useState<AgentProfile | null>(null);

  useEffect(() => {
    if (!editAgentId) { setEditProfile(null); return; }
    api.getAgent(editAgentId)
      .then(setEditProfile)
      .catch(() => setEditProfile(null));
  }, [editAgentId]);

  const handleAgentSubmit = useCallback(async (profile: AgentProfile) => {
    if (editAgentId) {
      await updateAgent(profile);
    } else {
      await createAgent(profile);
    }
    closeAgentModal();
    navigate(`/chat/${profile.id}`);
  }, [editAgentId, createAgent, updateAgent, closeAgentModal, navigate]);

  const handleAgentClone = useCallback(async () => {
    if (!editAgentId) return;
    const cloned = await cloneAgent(editAgentId);
    // Seed editProfile eagerly so the remounted modal shows cloned data without a flicker.
    setEditProfile(cloned);
    openEditAgentModal(cloned.id);
  }, [editAgentId, cloneAgent, openEditAgentModal]);

  const handleAgentDelete = useCallback(async (id: string) => {
    await api.deleteAgent(id);
    await useChatStore.getState().fetchAgents();
    const onDeletedThread = location.pathname === `/chat/${id}`;
    if (useNavigationStore.getState().getSelectedSubMenu("chat") === id) {
      useNavigationStore.getState().clearSelectedSubMenu("chat");
    }
    if (onDeletedThread) navigate("/chat", { replace: true });
  }, [location.pathname, navigate]);

  const startResizing = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    setIsResizing(true);
  }, []);

  const stopResizing = useCallback(() => {
    setIsResizing(false);
  }, []);

  const resize = useCallback(
    (e: MouseEvent) => {
      if (isResizing) {
        const newWidth = e.clientX - 78; // 4px outer pad + 60px main menu + 6px margin + 8px sidebar pad
        if (newWidth >= 120 && newWidth <= 600) {
          setSidebarWidthForView(activeViewId, newWidth);
        }
      }
    },
    [isResizing, activeViewId, setSidebarWidthForView]
  );

  useEffect(() => {
    if (isResizing) {
      window.addEventListener("mousemove", resize);
      window.addEventListener("mouseup", stopResizing);
      document.body.style.cursor = "col-resize";
    } else {
      window.removeEventListener("mousemove", resize);
      window.removeEventListener("mouseup", stopResizing);
      document.body.style.cursor = "default";
    }
    return () => {
      window.removeEventListener("mousemove", resize);
      window.removeEventListener("mouseup", stopResizing);
      document.body.style.cursor = "default";
    };
  }, [isResizing, resize, stopResizing]);

  const handleMainMenuClick = (viewId: ViewId) => {
    const config = viewConfigs.find((v) => v.id === viewId);
    if (!config) return;
    const lastSelected = useNavigationStore.getState().getSelectedSubMenu(viewId);
    navigate(lastSelected ? `${config.path}/${lastSelected}` : config.path);
  };

  const handleSubMenuClick = (subMenuSlug: string) => {
    if (!activeViewConfig) return;
    setSelectedSubMenu(activeViewId, subMenuSlug);
    navigate(`${activeViewConfig.path}/${subMenuSlug}`);
  };

  // Shared row markup for both the pinned Home row and every collapsible
  // nav rail row below it — `ref` is only ever passed for Home, so
  // useNavRailOverflow can measure its real rendered height.
  const renderNavRow = (view: ViewConfig, ref?: React.Ref<HTMLDivElement>) => {
    const Icon = view.icon;
    const isActive = activeViewId === view.id;
    return (
      <div
        key={view.id}
        ref={ref}
        className={twMerge(
          "w-full cursor-pointer py-[6px] flex flex-col items-center gap-[3px] transition-colors duration-150 group",
          isActive ? "text-[var(--text-primary)]" : "text-[var(--text-secondary)] hover:text-[var(--text-primary)]"
        )}
        onClick={() => handleMainMenuClick(view.id)}
      >
        <div className={twMerge(
          "w-[36px] h-[36px] flex items-center justify-center rounded-[10px] transition-colors",
          isActive ? "bg-[var(--bg-hover)]" : "group-hover:bg-[var(--bg-hover)]"
        )}>
          <Icon
            size={18}
            fill={isActive && !NO_FILL_ON_ACTIVE.has(view.id) ? "currentColor" : "none"}
            strokeWidth={isActive && NO_FILL_ON_ACTIVE.has(view.id) ? 2.5 : 2}
          />
        </div>
        <span className="text-[11px] font-semibold leading-none text-center truncate w-full px-[6px]">{view.label}</span>
      </div>
    );
  };

  const homeViewConfig = viewConfigs.find((v) => v.id === "home");

  return (
    <div
      className="w-full h-screen bg-[var(--bg-primary)] flex flex-col relative"
      style={{ backdropFilter: "var(--app-backdrop-filter, none)", WebkitBackdropFilter: "var(--app-backdrop-filter, none)" }}
    >
      {/* Draggable top bar */}
      <div data-tauri-drag-region className="absolute top-0 left-0 w-full h-[46px] z-10" />

      {/* Sidebar toggle — aligned with traffic lights */}
      <div
        className="absolute top-[13px] z-20"
        style={{
          transform: `translateX(${isFullscreen ? 13 : 90}px)`,
          transition: "transform 0.2s cubic-bezier(0.4, 0, 0.2, 1)",
        }}
      >
        <Tooltip label={sidebarOpen ? "Collapse sidebar" : "Expand sidebar"}>
          <div
            className="w-[20px] h-[20px] flex items-center justify-center rounded-[6px] text-[var(--text-secondary)] hover:bg-[var(--bg-hover)] cursor-pointer"
            onClick={toggleSidebar}
          >
            <SideBarIconClosed className="w-[18px] h-[18px]" />
          </div>
        </Tooltip>
      </div>

      <div className="flex flex-1 min-h-0">
        {/* Leftmost Icon Rail (Main Menu) — narrow, Slack-style column */}
        <div className="flex pt-[4px] pb-[4px] pl-[4px] flex-shrink-0">
          {/* Main Menu (Always visible) */}
          {/* pt-[51px]: the traffic lights live up top aligned with the top bar
              (back/forward + search row), so the first nav icon aligns with the
              *sidebar's* top edge instead — where its "Chat"-style title begins.
              That title starts at header-row(mt-3 + h-30 + mb-8 = 41) + sidebar
              pt-10 = 51px below this column's own pt-[4px], landing both at the
              same y. (Top-of-rail logo removed — it's now shown once, at the
              bottom of the rail, above Settings.) */}
          <div className="flex flex-col items-center gap-[6px] w-[60px] mr-[6px] pt-[51px]">
            {/* Workspace switcher tile — pinned above Home, outside the
                collapsible/overflow-accounted list below (it's always
                visible, never a candidate for the "More" overflow). Renders
                nothing until `/workspaces/active` resolves. Create/rename
                (via `WorkspaceEditModal`) is wired up internally — no props
                needed from this call site. */}
            <WorkspaceIndicator />

            {/* Nav items use Slack's looser 12px row-gap rather than this
                column's tighter 6px gap.

                flex-1 min-h-0 overflow-hidden: bounds this list to exactly
                the vertical space left above the bottom utility cluster
                (which keeps its natural size via the parent's mt-auto),
                instead of growing to fit its own content and pushing that
                cluster off-screen. That bounded, content-independent height
                is what useNavRailOverflow measures via containerRef — see
                its docstring for why that's required (a box whose height
                reacts to its own children would feedback-loop with the
                ResizeObserver deciding how many children to show). */}
            <div ref={navRailContainerRef} className="flex flex-col items-center w-full gap-[12px] flex-1 min-h-0 overflow-hidden">
              {/* Home is pinned — always visible, never part of the
                  collapse/overflow accounting. Its rendered height is what
                  the overflow hook measures a row's height off of. */}
              {homeViewConfig && renderNavRow(homeViewConfig, homeRowRef)}
              {visibleNavItems.map((view) => renderNavRow(view))}
              {overflowNavItems.length > 0 && (
                <div className="relative w-full" ref={moreIconRef}>
                  <div
                    role="button"
                    tabIndex={0}
                    aria-label="More"
                    className={twMerge(
                      "w-full cursor-pointer py-[6px] flex flex-col items-center gap-[3px] transition-colors duration-150 group",
                      moreOpen ? "text-[var(--text-primary)]" : "text-[var(--text-secondary)] hover:text-[var(--text-primary)]"
                    )}
                    onClick={() => setMoreOpen((o) => !o)}
                    onKeyDown={(e) => {
                      if (e.key === "Enter" || e.key === " ") {
                        e.preventDefault();
                        setMoreOpen((o) => !o);
                      }
                    }}
                  >
                    <div className={twMerge(
                      "w-[36px] h-[36px] flex items-center justify-center rounded-[10px] transition-colors",
                      moreOpen ? "bg-[var(--bg-hover)]" : "group-hover:bg-[var(--bg-hover)]"
                    )}>
                      <MoreHorizontal size={18} strokeWidth={2} />
                    </div>
                    <span className="text-[11px] font-semibold leading-none text-center truncate w-full px-[6px]">More</span>
                  </div>
                  <NavMorePopover
                    open={moreOpen}
                    onClose={() => setMoreOpen(false)}
                    onSelect={(id) => handleMainMenuClick(id as ViewId)}
                    items={overflowNavItems.map((v) => ({ id: v.id, label: v.label, icon: v.icon }))}
                    anchorRef={moreIconRef}
                  />
                </div>
              )}
            </div>

            {/* Bottom utility cluster — icon-only (tooltip on hover), no labels */}
            <div className="mt-auto flex flex-col items-center w-full gap-[12px]">
              <div className="relative w-full" ref={createIconRef}>
                <Tooltip label="Create" className="w-full justify-center">
                  <div
                    role="button"
                    tabIndex={0}
                    aria-label="Create"
                    className={twMerge(
                      "w-full cursor-pointer flex items-center justify-center transition-colors group",
                      createPopoverOpen ? "text-[var(--text-primary)]" : "text-[var(--text-secondary)]"
                    )}
                    onClick={() => setCreatePopoverOpen((o) => !o)}
                    onKeyDown={(e) => {
                      if (e.key === "Enter" || e.key === " ") {
                        e.preventDefault();
                        setCreatePopoverOpen((o) => !o);
                      }
                    }}
                  >
                    <div className={twMerge(
                      "w-[36px] h-[36px] flex items-center justify-center rounded-full bg-[var(--bg-hover)] transition-opacity",
                      createPopoverOpen ? "opacity-80" : "group-hover:opacity-80"
                    )}>
                      <Plus size={18} />
                    </div>
                  </div>
                </Tooltip>
                <CreatePopover
                  open={createPopoverOpen}
                  onClose={() => setCreatePopoverOpen(false)}
                  onSelect={(option) => {
                    if (option === "agent") useAgentProfileModalStore.getState().openNew();
                    if (option === "scheduled") useAssignmentEditorModalStore.getState().openCreate();
                  }}
                  anchorRef={createIconRef as RefObject<HTMLDivElement>}
                />
              </div>

              <Tooltip label="Learning" className="w-full justify-center">
                <div
                  role="button"
                  tabIndex={0}
                  aria-label="Learning"
                  className="w-full cursor-pointer flex items-center justify-center transition-colors text-[var(--text-secondary)] group"
                  onClick={() => openMemoriesWindow()}
                  onKeyDown={(e) => {
                    if (e.key === "Enter" || e.key === " ") {
                      e.preventDefault();
                      openMemoriesWindow();
                    }
                  }}
                >
                  <div className="w-[36px] h-[36px] flex items-center justify-center rounded-full bg-[var(--bg-hover)] transition-opacity group-hover:opacity-80">
                    <Brain size={17} />
                  </div>
                </div>
              </Tooltip>

              <div className="relative w-full mt-[8px] mb-[10px]" ref={settingsIconRef}>
                {/* Settings icon swapped for the Launchpad logo, in a rounded
                    SQUARE (not the rounded-full pill the other bottom-cluster
                    icons use). Behavior is untouched — same toggle, same
                    SettingsPopover. This is now the app's only rail logo — the
                    top-of-rail copy was removed as redundant.

                    Positioning is margin-based (mt-8/mb-10), not a transform:
                    a `transform` here would create a new containing block /
                    stacking context, which traps the Tooltip's and
                    SettingsPopover's `position: absolute` + high z-index
                    children so they paint *behind* the main content pane
                    instead of above it. mb-[10px] reproduces the old -10px
                    visual nudge (this box's own final position depends only
                    on its own margin-bottom, since it's the last item in a
                    bottom-anchored `mt-auto` cluster). mt-[8px] adds extra
                    clearance *above* it, which — by the same bottom-anchoring
                    — pushes Create/Learning up instead of moving this item,
                    so the outer ring's -inset-2 rest state (8px outset)
                    doesn't cut into the icon above.

                    Collections used to live in this rail as its own icon
                    (above Settings); it now lives inside SettingsPopover
                    instead, to free up vertical space on the strip — see
                    `onSelect` below. */}
                <Tooltip label="Settings" className="w-full justify-center">
                  <div
                    role="button"
                    tabIndex={0}
                    aria-label="Settings"
                    className={twMerge(
                      "w-full cursor-pointer flex items-center justify-center transition-colors group",
                      settingsPopoverOpen ? "text-[var(--text-primary)]" : "text-[var(--text-secondary)]"
                    )}
                    onClick={() => setSettingsPopoverOpen((o) => !o)}
                    onKeyDown={(e) => {
                      if (e.key === "Enter" || e.key === " ") {
                        e.preventDefault();
                        setSettingsPopoverOpen((o) => !o);
                      }
                    }}
                  >
                    <div className={twMerge(
                      "relative w-[40px] h-[40px] flex items-center justify-center rounded-[10px] bg-[#E3C4A3] transition-opacity",
                      settingsPopoverOpen ? "opacity-80" : "group-hover:opacity-80"
                    )}>
                      {/* Outer frame around the icon. Absolutely positioned with a
                          negative inset so it renders outside the 40x40 box's own
                          border-box — it never changes this div's layout size, so
                          toggling it can't shift the rail/siblings. On click it
                          animates from a gap around the inner box (`-inset-2`) to
                          flush against it (`inset-0`), i.e. it visually shrinks to
                          meet the inner box without any UI shift.

                          Radius is state-dependent (18px at rest, 10px when
                          shrunk), not a fixed value: the inner box is
                          rounded-[10px], so a ring sitting 8px outside it
                          (-inset-2) needs an 18px radius (10 + 8) to read as
                          concentric with it. When the ring shrinks flush
                          (inset-0) its radius animates down to 10px to match
                          the inner box exactly, so it reads as a literal
                          border on the inner div rather than a corner
                          mismatch. */}
                      {/* Opacity runs on its own timeline, offset from the
                          inset/radius shrink rather than synced to it — a
                          synced fade (plain transition-all) reads as "gone"
                          early because ease-in-out opacity drops below
                          visible contrast well before it's numerically at 0.

                          The offset is direction-dependent, deliberately
                          mirrored (not the same delay replayed forward) so
                          closing is the time-reverse of opening:
                          - Opening (shrinking to the logo): opacity holds at
                            100 for the first 60ms while the ring is still
                            travelling, then fades to 0 over the last 40ms —
                            it visually disappears right as it lands flush.
                          - Closing (expanding back out): opacity fades from
                            0 to 100 over the FIRST 40ms, then holds at 100
                            for the remaining 60ms while inset/radius keep
                            growing back to rest. Without this split, the
                            close would reuse the open's 60ms-delay and the
                            ring would stay invisible for 60% of the growth
                            before snapping in over the tail end. */}
                      <div
                        aria-hidden="true"
                        className={twMerge(
                          "absolute border-2 border-[var(--shell-border)] pointer-events-none",
                          settingsPopoverOpen ? "inset-0 rounded-[10px] opacity-0" : "-inset-2 rounded-[18px] opacity-100"
                        )}
                        style={{
                          transitionProperty: "inset, border-radius, opacity",
                          transitionDuration: "100ms, 100ms, 40ms",
                          transitionDelay: settingsPopoverOpen ? "0ms, 0ms, 60ms" : "0ms, 0ms, 0ms",
                          transitionTimingFunction: "ease-in-out",
                        }}
                      />
                      {/* Nude background reads light, so the logo is shown in
                          its original (non-inverted) color here — no filter. */}
                      <img
                        src={logo}
                        alt="Launchpad Logo"
                        className="relative w-[26px] h-[35px] object-contain"
                      />
                    </div>
                  </div>
                </Tooltip>
                <SettingsPopover
                  open={settingsPopoverOpen}
                  onClose={() => setSettingsPopoverOpen(false)}
                  onSelect={(action: SettingsPopoverAction) => {
                    if (action === "collections") {
                      openCollectionsModal();
                    } else {
                      setSettingsModalView(action);
                    }
                  }}
                  anchorRef={settingsIconRef as RefObject<HTMLDivElement>}
                />
              </div>
            </div>
          </div>
        </div>

        {/* Right side area containing both Top Header Bar and the content row (Sub-menu + Right Panel) */}
        <div className="flex flex-col flex-1 min-w-0 pt-[4px] pb-[4px] pr-[6px]">
          {/* Top Search Bar / Navigation Header */}
          <div className="flex justify-between h-[30px] items-center mb-[8px] mt-[3px] pl-[6px]">
            {/* Back / Forward navigation arrows + Breadcrumbs */}
            <div
              data-tauri-drag-region
              className={twMerge("flex items-center gap-1 relative z-20 flex-1 min-w-0", !sidebarOpen && "ml-[86px]", sidebarOpen && "ml-[66px]")}
            >
              <button
                type="button"
                className="w-[28px] h-[28px] flex items-center justify-center rounded-[6px] text-[var(--text-secondary)] hover:bg-[var(--bg-hover)] active:bg-[var(--bg-hover)] transition-colors duration-150 cursor-pointer"
                onClick={() => window.history.back()}
              >
                <ChevronLeft size={16} />
              </button>
              <button
                type="button"
                className="w-[28px] h-[28px] flex items-center justify-center rounded-[6px] text-[var(--text-secondary)] hover:bg-[var(--bg-hover)] active:bg-[var(--bg-hover)] transition-colors duration-150 cursor-pointer"
                onClick={() => window.history.forward()}
              >
                <ChevronRight size={16} />
              </button>

              {/* Breadcrumbs  */}
              {activeViewConfig && (
                <div data-tauri-drag-region className="flex items-center gap-1 ml-2 text-[13px] text-[var(--text-secondary)] flex-1 min-w-0 overflow-hidden whitespace-nowrap">
                  <span
                    className="hover:text-[var(--text-primary)] cursor-pointer transition-colors duration-150 shrink-0"
                    onClick={() => navigate(activeViewConfig.path)}
                  >
                    {activeViewConfig.label}
                  </span>
                  {activeSubMenuSlug && <BreadcrumbSubLabel viewId={activeViewId} slug={activeSubMenuSlug} subMenuItems={activeViewConfig.subMenuItems} />}
                </div>
              )}
            </div>

            <div className="flex items-center gap-2 relative z-50">
              <SearchBar />
            </div>
          </div>

          {/* Banner stack (offline, preferences, etc.) */}
          <BannerStack />

          {/* Update notification */}
          <UpdateNotification />

          {/* Content Row */}
          <div className="flex flex-1 min-h-0 relative">
            {/* Sub-menu (Collapsible Sidebar) */}
            <div
              ref={sidebarRef}
              className={twMerge(
                "flex flex-col pl-[8px] pt-[10px] overflow-hidden relative",
                sidebarOpen ? "rounded-l-[14px] border border-[var(--shell-border)]" : "w-0 pr-0 pl-0",
                "bg-[var(--bg-sidebar)]"
              )}
              style={{ width: sidebarOpen ? `${sidebarWidth}px` : 0 }}
            >
              {/* Sidebar title — reflects whichever top-level view is active */}
              {activeViewConfig && (
                <div className="pl-[4px] pr-[9px] mb-[10px] text-[20px] font-bold text-[var(--sidebar-text-primary,var(--text-primary))] truncate flex-shrink-0">
                  {activeViewConfig.label}
                </div>
              )}

              {/* Sub-menu Item List */}
              <div className={twMerge(
                "flex-1 min-h-0 pr-[5px]",
                activeViewId === "home" || activeViewId === "chat" || activeViewId === "tasks" || activeViewId === "projects" || activeViewId === "scheduled" || activeViewId === "assets"
                  ? "overflow-hidden flex flex-col"
                  : "overflow-y-auto",
                // Projects/Assets-only: cancel this wrapper's inherited 8px left
                // inset (this div itself has no left padding — the inset comes
                // from the outer sub-menu shell's `pl-[8px]` above, on
                // `sidebarRef`). `overflow-hidden` clips at a box's own padding
                // edge, so a descendant can only ever bleed into padding
                // declared on its *direct* clipping ancestor —
                // ProjectsSidebar.tsx/AssetsSidebar.tsx (two levels further in)
                // structurally cannot reach past this wrapper on their own.
                // Shifting this wrapper itself is the one place that can
                // actually cancel the outer shell's padding. Every other
                // sidebar keeps the normal 8px inset. Both sidebars add
                // matching +8px left margin to their search bar / header so
                // only the row list visually reaches the edge — see the
                // mirrored comment there.
                (activeViewId === "projects" || activeViewId === "assets") && "-ml-[8px]"
              )}>
                {/* Each of these five sidebars, once visited this session, stays
                    mounted for good — only `hidden` toggles. Revisiting a tab is
                    then a pure CSS show/hide with zero remount cost, instead of
                    tearing down and rebuilding the whole subtree (and, for Chat
                    specifically, re-opening up to three SSE connections) on every
                    single switch. Tabs never opened this session render nothing
                    here, same as before. */}
                {SPECIAL_SIDEBAR_VIEWS.filter((v) => visitedViews.has(v)).map((v) => (
                  <div key={v} className={v === activeViewId ? "flex-1 min-h-0 flex flex-col" : "hidden"}>
                    {v === "home" ? (
                      <HomeSidebar />
                    ) : v === "chat" ? (
                      <ChatSidebar />
                    ) : v === "projects" ? (
                      <ProjectsSidebar />
                    ) : v === "tasks" ? (
                      <TasksSidebar />
                    ) : v === "scheduled" ? (
                      <AssignmentsSidebar />
                    ) : (
                      <AssetsSidebar />
                    )}
                  </div>
                ))}
                {!SPECIAL_SIDEBAR_VIEWS.includes(activeViewId) && (
                  activeViewConfig?.subMenuItems.map((item, index) => {
                    if (item.isSectionHeader) {
                      return (
                        <div
                          key={item.id}
                          className={twMerge(
                            "px-2 pb-1 text-[11px] font-semibold text-[var(--text-secondary)] tracking-wider mt-4",
                            index === 0 && "mt-1"
                          )}
                        >
                          {item.label}
                        </div>
                      );
                    }

                    const isSelected = activeSubMenuSlug === item.id;
                    return (
                      <div
                        key={item.id}
                        className={twMerge(
                          "group text-[14px] p-[8px] flex items-center hover:bg-[var(--bg-hover)] cursor-pointer rounded-[12px] transition-all duration-150 mb-[2px]",
                          isSelected && "bg-[var(--bg-hover)] font-medium"
                        )}
                        onClick={() => handleSubMenuClick(item.id)}
                      >
                        <span className="flex-shrink-0 mr-2 w-[24px] h-[24px] flex items-center justify-center">
                          {item.icon ? (
                            <item.icon size={16} className="text-[var(--text-secondary)] group-hover:text-[var(--sidebar-text-primary,var(--text-primary))] transition-colors" />
                          ) : (
                            <span className="text-[16px]">{item.emoji ?? "#️⃣"}</span>
                          )}
                        </span>
                        <div className="flex flex-col min-w-0 flex-1">
                          <span className="text-ellipsis whitespace-nowrap overflow-hidden text-[var(--sidebar-text-primary,var(--text-primary))]">
                            {item.label}
                          </span>
                          {item.description && (
                            <span className="text-[11px] text-[var(--text-secondary)] font-normal leading-tight line-clamp-1">
                              {item.description}
                            </span>
                          )}
                        </div>
                      </div>
                    );
                  })
                )}
              </div>

              {/* Resizer Handle */}
              {sidebarOpen && (
                <div
                  className="absolute right-0 top-0 bottom-0 w-[6px] cursor-col-resize z-50 group"
                  onMouseDown={startResizing}
                >
                  <motion.div
                    initial={false}
                    animate={{
                      opacity: isResizing ? 1 : 0,
                      width: isResizing ? 2 : 1,
                    }}
                    whileHover={!isResizing ? {
                      opacity: 1,
                      transition: { delay: 0.05, duration: 0.01 }
                    } : {}}
                    className="absolute inset-y-0 right-0 rounded-full bg-[var(--accent)]"
                  />
                </div>
              )}
            </div>

            {/* Right Panel — content-panel neutralization for "chrome" themes
                (constant sidebar color across modes) is pure CSS, keyed on
                data-theme-kind/data-theme against the .app-content-panel
                class below (see App.css's Tier-B --surface-* contract). No
                per-theme JS here: the same root-level tokens resolve
                identically whether a consumer is docked in this panel or
                portaled to document.body. */}
            <div
              className={`flex flex-1 flex-col app-content-panel bg-[var(--bg-secondary)] overflow-hidden relative border-[var(--shell-border)]${sidebarOpen ? ' rounded-r-[14px] rounded-l-none border-t border-r border-b' : ' rounded-[14px] border'}`}
            >
              {/* Persistent SSE owner — outside Outlet so route transitions never
                  close the EventSource. Keeps streaming agents alive while the
                  user navigates between views. */}
              <SSEManager />
              {/* Route content — rendered through DeferredOutlet, which on a
                  top-level view switch withholds the new (often heavy) view for
                  exactly one animation frame so this shell paints first and the
                  view mounts a frame behind it ("UI first, then the dynamic
                  content"). Without that split, React commits the route change
                  and the new view's whole subtree in one synchronous pass —
                  ChatView alone brings a rich-text editor, a virtualized message
                  list, and several fetch-on-mount effects — which blocks the
                  shell from painting until it's all built, i.e. the Assets->Chat
                  stall. The frame swap itself still carries zero animation (no
                  crossfade, no exit fade); an earlier `AnimatePresence
                  mode="popLayout"` exit fade was removed for the same
                  "instantaneous" reason. Content-level readiness (has the new
                  view's own data loaded yet?) remains a separate concern handled
                  per-page via ContentGate/useReadyLatch — the only place a
                  fade-in still happens, once the real data is ready. Keyed on the
                  top-level view so this fires on nav-rail switches but not on
                  in-view sub-navigation (agent switches, project detail), which
                  stay mount-stable. */}
              <DeferredOutlet viewKey={activeViewId} />
            </div>
          </div>
        </div>
      </div>

      {/* Modals — every modal here derives its colors from the --modal-*
         CSS variables (set per data-app-theme + data-theme combo in
         App.css), not the plain --bg-secondary/--text-primary/etc. family.
         Modals are portaled or mounted as siblings of the main content
         panel above, so they never see that panel's light/dark override
         (scoped via inline style to one DOM node) — --modal-* exists
         precisely so modals get the same light-mode-white/dark-mode-chrome
         treatment without needing that. A new theme needs no changes here:
         it's correct the moment its App.css block exists (and, if it's a
         colorful "chrome" theme, once it adds a --modal-* light-mode
         override alongside midnight/sapphire/emerald/plum/gitlab/denim/
         goodstuff's). 'custom' is the one exception with no App.css block —
         its --modal-* light-mode override lives in the same shared selector
         list as the others (just the theme id, no colors needed there), and
         its base vars come from deriveCustomThemeVars() in App.tsx instead
         of a hardcoded block — see src/lib/customTheme.ts. */}
      <div>
        {/* Settings / Docs Modal */}
        <SettingsModal
          view={settingsModalView}
          onClose={() => setSettingsModalView(null)}
        />

        {/* Agent Profile Modal — both create and edit modes use AgentProfileModal
            for a consistent tabbed UI. Create mode is signaled by omitting `initial`. */}
        {isAgentModalOpen && !editAgentId && (
          <AgentProfileModal
            key="new"
            open
            onClose={closeAgentModal}
            onSubmit={handleAgentSubmit}
          />
        )}
        {isAgentModalOpen && editAgentId && editProfile && (
          <AgentProfileModal
            key={editAgentId}
            open
            initial={editProfile}
            onClose={closeAgentModal}
            onSubmit={handleAgentSubmit}
            onClone={handleAgentClone}
            onDelete={handleAgentDelete}
          />
        )}

        {/* Task Creation Modal */}
        <TaskCreateModal />

        {/* Competencies Modal */}
        <CompetenciesModal />

        {/* Assignments create/edit modal — single mount so Chat, Home, and the
            Assignments page can all open it via assignmentEditorModalStore. */}
        <AssignmentEditorModal />

        {/* Collections Modal (global plugin/skills/rules surface) */}
        <CollectionsModal />
      </div>
    </div>
  );
}

export default AppShell;
