import { useEffect, useMemo, useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { twMerge } from "tailwind-merge";
import { Search, Plus, FolderKanban, Activity, ListChecks, Trash2, ChevronDown, Check } from "lucide-react";
import type { LucideIcon } from "lucide-react";
import { useProjectStore } from "../../stores/projectStore";
import { useChatStore } from "../../stores/chatStore";
import { useNavigationStore } from "../../stores/navigationStore";
import { useIsDark, useUserPreferencesStore } from "../../stores/userPreferencesStore";
import { agentAvatarColor } from "../../lib/agentColors";
import * as api from "../../lib/api";
import { channel, subscribeChannel } from "../../lib/sseHub";
import ConfirmDialog from "../ui/ConfirmDialog";
import type { ProjectListItem } from "../../types/api";
import { ContentGate } from "../ContentGate";
import { SidebarListSkeleton } from "../shared/Skeletons";
import { useReadyLatch } from "../../hooks/useReadyLatch";

/** Status dot pinned to the avatar tile's bottom-right corner. Replaces the
 *  old colored-pill badge (which read as a mismatched color chip next to the
 *  coordinator name) — color/iconography now live on the avatar itself, and
 *  the meta line just shows the status as plain text (see below). Sized a
 *  bit larger (18px) than the top-right active-tasklist badge (14px,
 *  ChatSidebar-style ping) so the checkmark/magnifying-glass icons inside it
 *  have breathing room instead of rendering edge-to-edge with the circle. */
function StatusDot({ status }: { status: string }) {
  const colorMap: Record<string, string> = {
    active: "#00AF57", // emerald-600 — plain color, no icon
    interviewing: "#d97706", // amber-600 — magnifying glass
    completed: "#3186FF", // slate-500 — checkmark
    cancelled: "#FC413D", // slate-600
    draft: "#94a3b8", // slate-400
  };
  const bg = colorMap[status] ?? "#64748b";
  const Icon = status === "completed" ? Check : status === "interviewing" ? Search : null;
  return (
    <span
      aria-hidden
      className="absolute -bottom-[2px] -right-[2px] w-[18px] h-[18px] rounded-full flex items-center justify-center border-2 border-[var(--bg-secondary)]"
      style={{ backgroundColor: bg }}
    >
      {Icon && <Icon className="w-[9px] h-[9px] text-white" strokeWidth={3} />}
    </span>
  );
}

/** Toggles the sidebar stats bar (Total / Active / Tasklists / Top agent).
 *  Hidden per request; flip to `true` to bring the tiles back. Typed as a
 *  plain boolean so the gated JSX stays type-checked while it's off. */
const SHOW_STATS_BAR: boolean = false;

/** How the Recent Projects list is ordered. */
type SortMode = "created" | "alpha" | "tasklist";

const SORT_LABELS: Record<SortMode, string> = {
  created: "Date created",
  alpha: "Alphabetical",
  tasklist: "Last tasklist",
};

/** Sort selector — styled to match Settings → Memories' `AgentSortMenu`
 *  (a select-like box with a rotating chevron, opening a rounded popover
 *  where the active row is a solid accent highlight and the selected item
 *  carries a leading checkmark in accent text) rather than the plain
 *  hover-highlight chip this used to be. Kept compact (vs. that panel's
 *  full-width version) since it sits inline next to the "Recent Projects"
 *  header instead of under its own "Sort by" label. */
function SortMenu({ value, onChange }: { value: SortMode; onChange: (m: SortMode) => void }) {
  const [open, setOpen] = useState(false);
  return (
    <div className="relative flex-shrink-0">
      <button
        type="button"
        title="Sort projects"
        onClick={() => setOpen((o) => !o)}
        className="flex items-center gap-[6px] h-[24px] rounded-[6px] border border-[var(--border-primary)] bg-[var(--bg-primary)] px-[8px] text-[11px] font-medium normal-case tracking-normal text-[var(--text-secondary)] hover:border-[var(--text-secondary)] hover:text-[var(--sidebar-text-primary,var(--text-primary))] cursor-pointer transition-colors"
      >
        {SORT_LABELS[value]}
        <ChevronDown
          size={12}
          className={twMerge("flex-shrink-0 transition-transform", open && "rotate-180")}
        />
      </button>
      {open && (
        <>
          <div className="fixed inset-0 z-30" onClick={() => setOpen(false)} />
          <div className="absolute right-0 top-[calc(100%+4px)] z-40 min-w-[150px] rounded-[8px] border border-[var(--border-secondary)] bg-[var(--bg-secondary)] py-[4px] shadow-lg overflow-hidden">
            {(Object.keys(SORT_LABELS) as SortMode[]).map((mode) => (
              <button
                key={mode}
                type="button"
                onClick={() => {
                  onChange(mode);
                  setOpen(false);
                }}
                className={twMerge(
                  "flex w-full items-center gap-[8px] px-[12px] py-[8px] text-left text-[12px] normal-case tracking-normal cursor-pointer transition-colors",
                  "hover:bg-[var(--accent)] hover:text-white",
                  value === mode
                    ? "text-[var(--accent)] font-medium"
                    : "text-[var(--text-secondary)]",
                )}
              >
                <Check size={12} className={value === mode ? "opacity-100" : "opacity-0"} />
                {SORT_LABELS[mode]}
              </button>
            ))}
          </div>
        </>
      )}
    </div>
  );
}

/** A single rounded stat tile: tinted icon chip on top, value + label below. */
function StatCard({
  icon: Icon,
  tint,
  value,
  label,
}: {
  icon: LucideIcon;
  tint: string;
  value: string | number;
  label: string;
}) {
  return (
    <div className="flex flex-col gap-[8px] rounded-[12px] border border-[var(--border-secondary)] bg-[var(--bg-secondary)] px-[10px] py-[10px]">
      <span className={twMerge("flex h-[24px] w-[24px] items-center justify-center rounded-[8px]", tint)}>
        <Icon size={14} />
      </span>
      <div>
        <div className="truncate text-[16px] font-semibold leading-tight text-[var(--sidebar-text-primary,var(--text-primary))]">
          {value}
        </div>
        <div className="mt-[2px] text-[11px] text-[var(--text-secondary)]">{label}</div>
      </div>
    </div>
  );
}

export function ProjectsSidebar() {
  const navigate = useNavigate();
  // The active project route is `/projects/:projectId` (see App.tsx), so read
  // `projectId` here — not `subMenuSlug`, which only exists for other views and
  // would always be undefined, leaving no tile highlighted.
  const { projectId } = useParams<{ projectId?: string }>();
  const setSelectedSubMenu = useNavigationStore((s) => s.setSelectedSubMenu);
  const isDark = useIsDark();
  const circularAvatars = useUserPreferencesStore((s) => s.circularAvatars);
  const [search, setSearch] = useState("");
  const [sortMode, setSortMode] = useState<SortMode>("created");
  const [pendingDelete, setPendingDelete] = useState<ProjectListItem | null>(null);

  const projects = useProjectStore((s) => s.projects);
  const projectsLoading = useProjectStore((s) => s.projectsLoading);
  const fetchProjects = useProjectStore((s) => s.fetchProjects);
  const deleteProject = useProjectStore((s) => s.deleteProject);
  const agents = useChatStore((s) => s.agents);
  const fetchAgents = useChatStore((s) => s.fetchAgents);

  useEffect(() => {
    fetchProjects();
  }, [fetchProjects]);

  useEffect(() => {
    if (agents.length === 0) fetchAgents();
  }, [agents.length, fetchAgents]);

  // Live system channel: refresh agents whenever a tasklist lifecycle event or
  // snapshot update fires so the coordinator ping appears without navigation.
  useEffect(() => {
    const refresh = () => fetchAgents();
    const sub = subscribeChannel(channel.system(), {
      listeners: {
        "tasklist.created": refresh,
        "tasklist.status_changed": refresh,
        "tasklist.completed": refresh,
        "tasklist.failed": refresh,
        "agent.snapshot_updated": refresh,
      },
    });
    return () => sub.close();
  }, [fetchAgents]);

  // Aggregate tasklist count across all projects, plus each project's most
  // recent tasklist creation time (epoch ms, 0 if none) for the "Last tasklist"
  // sort. Fetched once per projects change; failures count as zero so a single
  // bad project can't blank the stat or skew the ordering.
  const [tasklistCount, setTasklistCount] = useState<number | null>(null);
  const [latestTasklistAt, setLatestTasklistAt] = useState<Record<string, number>>({});
  useEffect(() => {
    if (projects.length === 0) {
      setTasklistCount(0);
      setLatestTasklistAt({});
      return;
    }
    let cancelled = false;
    (async () => {
      const results = await Promise.all(
        projects.map(async (p) => {
          try {
            const resp = await api.listTasklistsForScope({ kind: "project", id: p.id });
            const lists = [...(resp.active ? [resp.active] : []), ...resp.recent];
            let latest = 0;
            for (const tl of lists) {
              const t = Date.parse(tl.created_at);
              if (!Number.isNaN(t) && t > latest) latest = t;
            }
            return { id: p.id, count: lists.length, latest };
          } catch {
            return { id: p.id, count: 0, latest: 0 };
          }
        }),
      );
      if (cancelled) return;
      setTasklistCount(results.reduce((a, r) => a + r.count, 0));
      const map: Record<string, number> = {};
      for (const r of results) map[r.id] = r.latest;
      setLatestTasklistAt(map);
    })();
    return () => {
      cancelled = true;
    };
  }, [projects]);

  // The agent coordinating the most projects — the "go-to" agent.
  const topAgent = useMemo(() => {
    if (projects.length === 0) return null;
    const counts = new Map<string, number>();
    for (const p of projects) counts.set(p.agent_id, (counts.get(p.agent_id) ?? 0) + 1);
    let bestId: string | null = null;
    let best = 0;
    for (const [id, count] of counts) {
      if (count > best) {
        best = count;
        bestId = id;
      }
    }
    if (!bestId) return null;
    const agent = agents.find((a) => a.agent_id === bestId);
    return {
      name: agent?.name ?? "Unknown",
      emoji: agent?.emoji ?? "\u{1F916}",
    };
  }, [projects, agents]);

  // Filter by search, then order by the selected sort mode. "created" and
  // "tasklist" are newest-first; "tasklist" falls back to creation time when a
  // project has no tasklists (or its fetch failed) so ties stay deterministic.
  const visibleProjects = useMemo(() => {
    const q = search.trim().toLowerCase();
    const arr = q ? projects.filter((p) => p.name.toLowerCase().includes(q)) : [...projects];
    switch (sortMode) {
      case "alpha":
        arr.sort((a, b) => a.name.localeCompare(b.name, undefined, { sensitivity: "base" }));
        break;
      case "tasklist":
        arr.sort((a, b) => {
          const ta = latestTasklistAt[a.id] ?? 0;
          const tb = latestTasklistAt[b.id] ?? 0;
          if (tb !== ta) return tb - ta;
          return Date.parse(b.created_at) - Date.parse(a.created_at);
        });
        break;
      case "created":
      default:
        arr.sort((a, b) => Date.parse(b.created_at) - Date.parse(a.created_at));
        break;
    }
    return arr;
  }, [projects, search, sortMode, latestTasklistAt]);

  const activeCount = projects.filter(
    (p) => p.status === "active" || p.status === "interviewing",
  ).length;

  const ready = useReadyLatch(projects.length > 0, projectsLoading);

  return (
    <div className="flex flex-col flex-1 min-h-0">
      {/* Search input — same surface styling as ChatSidebar/HomeSidebar
          (app-search-surface + --search-bg/--search-border tokens), sitting
          outside the scrollable region below so it never scrolls away.
          `ml-[12px]` = the usual 4px own-inset + 8px compensating for
          AppShell.tsx's Projects-only `-ml-[8px]` on the sub-menu wrapper
          (see comment there) — keeps this bar's visual position unchanged
          even though its containing box now starts flush with the true
          sidebar edge instead of 8px in. */}
      <div className="ml-[12px] mr-[4px] mb-[8px] flex items-center gap-2">
        <div className="app-search-surface cursor-text border-[1px] border-[var(--search-border)] h-[32px] flex-1 flex items-center gap-1 px-[10px] rounded-[8px] bg-[var(--search-bg)] text-[var(--text-secondary)]">
          <Search className="w-[14px] h-[14px] text-[var(--text-secondary)] flex-shrink-0" />
          <input
            type="text"
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            placeholder="Find project..."
            className="flex-1 text-[15px] leading-[1.4667] bg-transparent outline-none text-[var(--sidebar-text-primary,var(--text-primary))] placeholder:text-[var(--text-secondary)]"
          />
        </div>
      </div>

      {/* Scrollable body: stats bar, Recent Projects header + list.
          `-mr-[5px]` cancels the *sidebar's own* shared `pr-[5px]`
          (AppShell.tsx's sub-menu wrapper, shared by every sidebar) — that
          padding sits one level further up than ContentGate's pr-[5px], and
          since this div is the one carrying `overflow-x-hidden`, nothing
          inside it (row, divider, ...) can ever visually bleed past its own
          box no matter how far a descendant's negative offset reaches. The
          only way to move that clip boundary is to widen this box itself. */}
      <div className="flex-1 min-h-0 overflow-y-auto overflow-x-hidden -mr-[5px]">
        {/* Stats bar hidden per request — set SHOW_STATS_BAR to true to restore
            the Total / Active / Tasklists / Top agent tiles. */}
        {SHOW_STATS_BAR && (
          // ml-[12px]: same AppShell -ml-[8px] compensation as the search bar above.
          <div className="ml-[12px] mr-[4px] mb-[10px] grid grid-cols-2 gap-[8px]">
            <StatCard
              icon={FolderKanban}
              tint="bg-blue-500/15 text-blue-500"
              value={projects.length}
              label="Total"
            />
            <StatCard
              icon={Activity}
              tint="bg-emerald-500/15 text-emerald-500"
              value={activeCount}
              label="Active"
            />
            <StatCard
              icon={ListChecks}
              tint="bg-violet-500/15 text-violet-500"
              value={tasklistCount ?? "–"}
              label="Tasklists"
            />
            {/* Top agent tile — emoji stands in for the icon chip */}
            <div className="flex flex-col gap-[8px] rounded-[12px] border border-[var(--border-secondary)] bg-[var(--bg-secondary)] px-[10px] py-[10px]">
              <span className="flex h-[24px] w-[24px] items-center justify-center rounded-[8px] bg-amber-500/15 text-[13px] leading-none">
                {topAgent ? topAgent.emoji : "\u{1F916}"}
              </span>
              <div>
                <div className="truncate text-[13px] font-semibold leading-tight text-[var(--sidebar-text-primary,var(--text-primary))]">
                  {topAgent ? topAgent.name : "–"}
                </div>
                <div className="mt-[2px] text-[11px] text-[var(--text-secondary)]">Top agent</div>
              </div>
            </div>
          </div>
        )}

        {/* Recent Projects — ml-[12px]: same AppShell -ml-[8px] compensation
            as the search bar above, so this header stays put while the row
            list below it (inside ContentGate) is the one part that's meant
            to shift and bleed to the true edge. */}
        <div className="ml-[12px] mr-[4px] mb-[4px] flex items-center justify-between gap-2 px-[4px]">
          <span className="text-[11px] font-semibold uppercase tracking-wider text-[var(--text-secondary)]">
            Recent Projects
          </span>
          <SortMenu value={sortMode} onChange={setSortMode} />
        </div>
        <ContentGate ready={ready} skeleton={<SidebarListSkeleton rows={4} />} className="pr-[5px]">
          {visibleProjects.length === 0 ? (
            <div className="px-[8px] py-[10px] text-[13px] text-[var(--text-secondary)]">
              {search ? "No projects found" : "No projects yet."}
            </div>
          ) : (
            visibleProjects.map((project) => {
              const isSelected = projectId === project.id;
              const coordinatorAgent = agents.find((a) => a.agent_id === project.agent_id);
              return (
                <div
                  key={project.id}
                  onClick={() => {
                    setSelectedSubMenu("projects", project.id);
                    navigate(`/projects/${project.id}`);
                  }}
                  className="group isolate relative mx-[4px] flex items-start gap-3 px-[8px] py-[12px] cursor-pointer transition-colors duration-150"
                >
                  {/* Edge-to-edge hover/active highlight. Lives as its own layer
                    (not the row's own background) so it can bleed past the
                    row's box using the exact same -left-[4px]/-right-[9px]
                    offsets as the divider below (row's own mx-[4px] + Content-
                    Gate's pr-[5px] — keep these two in sync if either changes).
                    `isolate` on the row + `-z-10` here pin it to the bottom of
                    the row's own paint order so it stays behind the avatar and
                    text instead of covering them. */}
                  <div
                    aria-hidden
                    className={twMerge(
                      "absolute inset-y-0 -left-[4px] -right-[9px] -z-10 transition-colors duration-150",
                      isSelected ? "bg-[var(--sidebar-active-bg)]" : "group-hover:bg-[var(--bg-hover)]"
                    )}
                  />
                  <div className="relative flex-shrink-0">
                    <span
                      className="flex h-[40px] w-[40px] items-center justify-center rounded-[4px] text-[var(--sidebar-text-primary,var(--text-primary))]"
                      style={{ backgroundColor: agentAvatarColor(project.name, isDark) }}
                    >
                      {project.emoji ? (
                        <span className="text-[20px] leading-none">{project.emoji}</span>
                      ) : (
                        <FolderKanban size={20} />
                      )}
                    </span>
                    <StatusDot status={project.status} />
                    {coordinatorAgent?.active_tasklist_title && !isSelected && (
                      <>
                        <span
                          className="absolute inset-0 rounded-[8px] animate-ping pointer-events-none"
                          style={{
                            boxShadow: "0 0 0 3px color-mix(in srgb, var(--accent) 65%, transparent)",
                            backgroundColor: "color-mix(in srgb, var(--accent) 15%, transparent)",
                          }}
                          aria-hidden
                        />
                        <span className="absolute -top-[2px] -right-[2px] w-[14px] h-[14px] rounded-full bg-[var(--accent)] flex items-center justify-center border-2 border-[var(--bg-secondary)]">
                          <ListChecks className="w-[8px] h-[8px] text-white" />
                        </span>
                      </>
                    )}
                  </div>
                  <div className="flex min-w-0 flex-1 flex-col gap-[2px]">
                    <span className="truncate min-w-0 text-[15px] font-semibold text-[var(--sidebar-text-primary,var(--text-primary))]">
                      {project.name}
                    </span>
                    <div className="flex items-center gap-[6px] min-w-0">
                      <span className="flex-shrink-0 text-[11px] font-medium capitalize text-[var(--text-secondary)]">
                        {project.status}
                      </span>
                      {coordinatorAgent && (
                        <>
                          {/* Divider between status and agent details — same
                            border token as the row divider below, just a
                            short vertical stroke instead of a full-width one. */}
                          <span aria-hidden className="flex-shrink-0 w-[1px] h-[10px] bg-[var(--border-primary)]" />
                          <span className="flex items-center gap-[4px] min-w-0 text-[11px] text-[var(--text-secondary)]">
                            <span
                              className={twMerge(
                                "flex h-[16px] w-[16px] flex-shrink-0 items-center justify-center text-[10px] leading-none",
                                circularAvatars ? "rounded-full" : "rounded-[4px]",
                              )}
                              style={{ backgroundColor: agentAvatarColor(coordinatorAgent.name, isDark) }}
                            >
                              {coordinatorAgent.emoji ?? "\u{1F916}"}
                            </span>
                            <span className="truncate">{coordinatorAgent.name}</span>
                          </span>
                        </>
                      )}
                    </div>
                    {coordinatorAgent?.active_tasklist_title && (
                      <div className="flex items-center gap-[4px] min-w-0">
                        <ListChecks className="w-[11px] h-[11px] flex-shrink-0 text-[var(--accent)]" />
                        <span className="text-[11px] truncate text-[var(--sidebar-text-primary,var(--text-primary))]">
                          {coordinatorAgent.active_tasklist_title}
                        </span>
                      </div>
                    )}
                  </div>
                  {/* `-right-[1px]`: reverted off the previous `-right-[13px]`,
                    which overshot this list's actual clip boundary and cut
                    the icon off. The scroll wrapper on line 301 carries
                    `overflow-x-hidden`, and the divider/highlight layers
                    (above and below) prove that boundary sits at exactly
                    `-right-[9px]` from this row's own box — row's own
                    mx-[4px] + ContentGate's pr-[5px] stacked — since they
                    bleed flush to it without ever being clipped themselves.
                    `-13px` bled 4px past that boundary, slicing the right
                    edge of the trash icon. `-1px` lands 8px inside the
                    boundary, matching the `top-[8px]` gap. The prior
                    "~20px on the right" screenshot measurement that
                    justified `-13px` doesn't hold up — don't push this past
                    `-right-[9px]`, that number is the real ceiling. */}
                  <button
                    title="Delete project"
                    onClick={(e) => { e.stopPropagation(); setPendingDelete(project); }}
                    className="absolute -right-[1px] top-[8px] flex h-[24px] w-[24px] items-center justify-center rounded-[6px] bg-[var(--bg-secondary)] border border-[var(--border-secondary)] shadow-sm text-[var(--text-secondary)] opacity-0 transition-opacity hover:bg-red-500/10 hover:text-red-500 group-hover:opacity-100 cursor-pointer"
                  >
                    <Trash2 size={14} />
                  </button>
                  {/* Same -left-[4px]/-right-[9px] bleed as the highlight layer
                    above (row's own mx-[4px] + ContentGate's pr-[5px] on the
                    right). The left side now reaches the sidebar's true edge:
                    AppShell.tsx's sub-menu wrapper cancels its own inherited
                    8px inset with a Projects-only `-ml-[8px]` (see comment
                    there), so `-left-[4px]` here only has this row's own
                    mx-[4px] left to cancel — the same one-hop bleed the right
                    side already used. */}
                  <div className="absolute bottom-0 -left-[4px] -right-[9px] border-b border-[var(--border-primary)] group-last:hidden" />
                </div>
              );
            })
          )}
        </ContentGate>
      </div>

      {/* New Project button — pinned as a footer below the scrollable list
          instead of scrolling with it, so it's reachable without scrolling
          all the way down through a long project list. */}
      <div className="flex-shrink-0 pt-[10px] pb-[10px] mt-[4px] border-t border-[var(--border-secondary)]">
        <button
          onClick={() => navigate("/projects/new")}
          className={twMerge(
            // ml-[12px]/w-[calc(100%-16px)]: same AppShell -ml-[8px]
            // compensation as the search bar above (12 = 4 own-inset + 8
            // compensating; width drops the matching extra 8px so the
            // right edge doesn't shift).
            "ml-[12px] mr-[4px] w-[calc(100%-16px)] flex items-center justify-center gap-2 h-[36px] rounded-[10px]",
            "bg-[var(--text-primary)] text-[var(--bg-secondary)] text-[13px] font-semibold whitespace-nowrap",
            "hover:opacity-90 transition-opacity cursor-pointer"
          )}
        >
          <Plus className="w-[16px] h-[16px]" />
          New Project
        </button>
      </div>
      <ConfirmDialog
        open={!!pendingDelete}
        destructive
        title="Delete project"
        confirmLabel="Delete"
        message={pendingDelete ? `Delete "${pendingDelete.name}"? This permanently removes the project and cannot be undone.` : ""}
        onCancel={() => setPendingDelete(null)}
        onConfirm={async () => {
          const p = pendingDelete;
          if (!p) return;
          await deleteProject(p.id);
          setPendingDelete(null);
          if (useNavigationStore.getState().getSelectedSubMenu("projects") === p.id) {
            useNavigationStore.getState().clearSelectedSubMenu("projects");
          }
          if (projectId === p.id) navigate("/projects/new");
        }}
      />
    </div>
  );
}
