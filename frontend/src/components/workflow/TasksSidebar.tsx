import { useEffect, useMemo, useState } from "react";
import { useNavigate, useLocation } from "react-router-dom";
import { twMerge } from "tailwind-merge";
import { Hash, Search, FolderInput, RefreshCw, Star, Clock } from "lucide-react";
import { open as tauriOpen } from "@tauri-apps/plugin-dialog";
import { useWorkflowStore } from "../../stores/workflowStore";
import { useNavigationStore } from "../../stores/navigationStore";
import { useUserPreferencesStore } from "../../stores/userPreferencesStore";
import { getViewConfig } from "../../config/navigation";
import type { WorkflowSummary } from "../../types/workflow";
import * as api from "../../lib/api";
import { ContentGate } from "../ContentGate";
import { SidebarListSkeleton } from "../shared/Skeletons";
import { useReadyLatch } from "../../hooks/useReadyLatch";

/** Cap on the sidebar's "Recent" quick-access rows — mirrors the Workflows catalog tab's cap (TasksView), sized down for the narrower sidebar. */
const MAX_RECENT_SIDEBAR_WORKFLOWS = 5;

/** Single workflow row, shared by the Starred / Recent / full-list sections so all three stay visually identical. */
function WorkflowRow({
  workflow,
  isSelected,
  starred,
  onToggleStar,
  onClick,
}: {
  workflow: WorkflowSummary;
  isSelected: boolean;
  starred: boolean;
  onToggleStar: () => void;
  onClick: () => void;
}) {
  return (
    <div
      className={twMerge(
        "group text-[14px] px-2 py-[5px] flex items-center hover:bg-[var(--bg-hover)] cursor-pointer rounded-[8px] transition-all duration-150",
        isSelected && "bg-[var(--sidebar-active-bg)] font-medium"
      )}
      onClick={onClick}
    >
      <span className="flex-shrink-0 mr-1 w-[16px] h-[16px] flex items-center justify-center">
        <Hash size={13} className={twMerge("group-hover:text-[var(--text-secondary)] transition-colors", isSelected ? "text-[var(--sidebar-active-text-secondary)]" : "text-[var(--text-tertiary)]")} />
      </span>
      <span className={twMerge("truncate flex-1", isSelected ? "text-[var(--sidebar-active-text-primary)]" : "text-[var(--sidebar-text-primary,var(--text-primary))]")}>{workflow.name}</span>
      <button
        type="button"
        onClick={(e) => {
          e.stopPropagation();
          onToggleStar();
        }}
        title={starred ? "Unstar workflow" : "Star workflow"}
        aria-label={starred ? "Unstar workflow" : "Star workflow"}
        className={twMerge(
          "flex-shrink-0 ml-1 flex items-center justify-center w-[18px] h-[18px] rounded transition-colors cursor-pointer",
          starred
            ? "text-[var(--accent)] opacity-100"
            : "text-[var(--text-tertiary)] opacity-0 group-hover:opacity-100 hover:text-[var(--text-primary)]"
        )}
      >
        <Star size={12} fill={starred ? "currentColor" : "none"} />
      </button>
    </div>
  );
}

/** Small uppercase section header (Starred / Recent / All workflows) matching the sidebar's existing "Workflows" header styling. */
function WorkflowSectionHeader({ icon: Icon, title }: { icon: typeof Star; title: string }) {
  return (
    <div className="flex items-center gap-1 px-2 pb-0.5 pt-1.5">
      <Icon size={10} className="text-[var(--text-tertiary)]" />
      <span className="text-[10px] font-semibold text-[var(--text-tertiary)] uppercase tracking-wider">
        {title}
      </span>
    </div>
  );
}

export function TasksSidebar() {
  const navigate = useNavigate();
  const location = useLocation();
  const workflows = useWorkflowStore((s) => s.workflows);
  // `loading` is a store-wide flag shared with unrelated task mutations
  // (fetchTasks/fetchTask/startTask/...) — useReadyLatch only cares about it
  // long enough to see one true→false edge, then ignores later flaps, so
  // starting/archiving a task elsewhere in the app can never re-blank this list.
  const workflowsLoading = useWorkflowStore((s) => s.loading);
  const fetchWorkflows = useWorkflowStore((s) => s.fetchWorkflows);
  const setSelectedSubMenu = useNavigationStore((s) => s.setSelectedSubMenu);
  const starredWorkflowIds = useUserPreferencesStore((s) => s.starredWorkflowIds);
  const toggleStarredWorkflow = useUserPreferencesStore((s) => s.toggleStarredWorkflow);
  const [search, setSearch] = useState("");
  const [importing, setImporting] = useState(false);
  const [refreshing, setRefreshing] = useState(false);

  useEffect(() => {
    if (workflows.length === 0) fetchWorkflows();
  }, [workflows.length, fetchWorkflows]);

  const tasksConfig = getViewConfig("tasks");
  const staticItems = tasksConfig?.subMenuItems ?? [];

  // Derive active state from URL
  const pathSegment = location.pathname.replace("/tasks/", "").split("/")[0];
  const isNewTask = location.pathname.startsWith("/tasks/new");
  const newWorkflowParam = isNewTask ? new URLSearchParams(location.search).get("workflow") : null;

  // Filter workflows by search
  const filteredWorkflows = search.trim()
    ? workflows.filter((wf) =>
      wf.name.toLowerCase().includes(search.toLowerCase()) ||
      wf.id.toLowerCase().includes(search.toLowerCase())
    )
    : workflows;

  // Quick-access groups, mirroring the Workflows catalog tab (TasksView) —
  // only shown while not searching, so they don't duplicate the search
  // results rendered below.
  const isBrowsing = !search.trim();
  const starredWorkflows = useMemo(
    () => filteredWorkflows.filter((wf) => starredWorkflowIds.includes(wf.id)),
    [filteredWorkflows, starredWorkflowIds],
  );
  const recentWorkflows = useMemo(() => {
    return [...filteredWorkflows]
      .filter((wf) => !!wf.last_run)
      .sort((a, b) => new Date(b.last_run!).getTime() - new Date(a.last_run!).getTime())
      .slice(0, MAX_RECENT_SIDEBAR_WORKFLOWS);
  }, [filteredWorkflows]);
  const hasQuickAccess = isBrowsing && (starredWorkflows.length > 0 || recentWorkflows.length > 0);

  const handleStaticClick = (id: string) => {
    setSelectedSubMenu("tasks", id);
    navigate(`/tasks/${id}`);
  };

  const handleWorkflowClick = (workflowId: string) => {
    setSelectedSubMenu("tasks", `new-${workflowId}`);
    navigate(`/tasks/new?workflow=${encodeURIComponent(workflowId)}`);
  };

  const handleImport = async () => {
    try {
      const selected = await tauriOpen({ directory: true, multiple: false });
      if (!selected) return;
      setImporting(true);
      await api.importWorkflow(selected as string);
      await fetchWorkflows();
    } catch (err) {
      console.error("Import failed:", err);
    } finally {
      setImporting(false);
    }
  };

  const ready = useReadyLatch(workflows.length > 0, workflowsLoading);

  const handleRefresh = async () => {
    setRefreshing(true);
    try {
      await api.refreshWorkflows();
      await fetchWorkflows();
    } catch (err) {
      console.error("Refresh failed:", err);
    } finally {
      setRefreshing(false);
    }
  };

  return (
    <div className="flex flex-col flex-1 min-h-0">
      {/* Search */}
      <div className="mx-[4px] mb-[8px] flex items-center gap-2 relative z-20">
        <div className="cursor-text border-[1px] border-[var(--border-secondary)] h-[32px] flex-1 flex items-center gap-1 px-[10px] rounded-[8px] bg-[var(--bg-secondary)] text-[var(--text-secondary)]">
          <Search className="w-[14px] h-[14px] text-[var(--text-secondary)] flex-shrink-0" />
          <input
            type="text"
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            placeholder="Find workflow..."
            className="flex-1 text-[15px] leading-[1.4667] bg-transparent outline-none text-[var(--sidebar-text-primary,var(--text-primary))] placeholder:text-[var(--text-secondary)]"
          />
        </div>
      </div>

      {/* Everything below search scrolls as one unit (static items, Import,
          and the workflow list) — mirrors ChatSidebar, where only the search
          box sits outside the ContentGate/scroll wrapper. */}
      <ContentGate
        ready={ready}
        skeleton={<SidebarListSkeleton rows={3} />}
        className="flex-1 min-h-0 flex flex-col overflow-y-auto pr-[5px]"
      >
        {/* Static items */}
        {staticItems.map((item) => {
          const isSelected = pathSegment === item.id;
          return (
            <div
              key={item.id}
              className={twMerge(
                "group text-[14px] px-2 py-[5px] flex items-center hover:bg-[var(--bg-hover)] cursor-pointer rounded-[8px] transition-all duration-150",
                isSelected && "bg-[var(--sidebar-active-bg)] font-medium"
              )}
              onClick={() => handleStaticClick(item.id)}
            >
              <span className="flex-shrink-0 mr-2 w-[20px] h-[20px] flex items-center justify-center">
                {item.icon && (
                  <item.icon size={14} className={twMerge("group-hover:text-[var(--sidebar-text-primary,var(--text-primary))] transition-colors", isSelected ? "text-[var(--sidebar-active-text-secondary)]" : "text-[var(--text-secondary)]")} />
                )}
              </span>
              <span className={twMerge("truncate", isSelected ? "text-[var(--sidebar-active-text-primary)]" : "text-[var(--sidebar-text-primary,var(--text-primary))]")}>{item.label}</span>
            </div>
          );
        })}

        {/* Divider + Workflows header */}
        <div className="h-[1px] bg-[var(--border-secondary)] my-1.5 mx-1" />
        <div className="group/wfheader flex items-center justify-between px-2 pb-0.5 pt-0.5">
          <span className="text-[10px] font-semibold text-[var(--text-tertiary)] uppercase tracking-wider">
            Workflows
          </span>
          <button
            onClick={handleRefresh}
            disabled={refreshing}
            title="Refresh workflows"
            className="p-0.5 rounded text-[var(--text-tertiary)] hover:text-[var(--text-secondary)] hover:bg-[var(--bg-hover)] transition-all cursor-pointer disabled:opacity-50 opacity-0 group-hover/wfheader:opacity-100"
          >
            <RefreshCw size={11} className={refreshing ? "animate-spin" : ""} />
          </button>
        </div>

        {/* Import workflow */}
        <div
          onClick={handleImport}
          className="group text-[14px] px-2 py-[5px] flex items-center hover:bg-[var(--bg-hover)] cursor-pointer rounded-[8px] transition-all duration-150"
        >
          <span className="flex-shrink-0 mr-1 w-[16px] h-[16px] flex items-center justify-center">
            <FolderInput size={13} className="text-[var(--text-secondary)]" />
          </span>
          <span className="truncate text-[var(--text-secondary)]">{importing ? "Importing..." : "Import workflow"}</span>
        </div>

        {isBrowsing ? (
          <>
            {/* Browsing (not searching): only the quick-access Starred/Recent
                rows — the exhaustive workflow list already lives one click
                away on the "Workflows" catalog tab (staticItems above), so
                duplicating every workflow here just adds noise/scroll. */}
            {starredWorkflows.length > 0 && (
              <>
                <WorkflowSectionHeader icon={Star} title="Starred" />
                {starredWorkflows.map((wf) => (
                  <WorkflowRow
                    key={`starred-${wf.id}`}
                    workflow={wf}
                    isSelected={newWorkflowParam === wf.id}
                    starred
                    onToggleStar={() => toggleStarredWorkflow(wf.id)}
                    onClick={() => handleWorkflowClick(wf.id)}
                  />
                ))}
              </>
            )}

            {recentWorkflows.length > 0 && (
              <>
                <WorkflowSectionHeader icon={Clock} title="Recent" />
                {recentWorkflows.map((wf) => (
                  <WorkflowRow
                    key={`recent-${wf.id}`}
                    workflow={wf}
                    isSelected={newWorkflowParam === wf.id}
                    starred={starredWorkflowIds.includes(wf.id)}
                    onToggleStar={() => toggleStarredWorkflow(wf.id)}
                    onClick={() => handleWorkflowClick(wf.id)}
                  />
                ))}
              </>
            )}

            {!hasQuickAccess && (
              <div className="px-2 py-2 text-[12px] text-[var(--text-tertiary)]">
                No starred or recent workflows yet
              </div>
            )}
          </>
        ) : (
          <>
            {/* Actively searching: still search across every workflow, not
                just the quick-access set — "Find workflow..." is a jump-to
                feature, not scoped to Starred/Recent. */}
            {filteredWorkflows.map((wf) => (
              <WorkflowRow
                key={wf.id}
                workflow={wf}
                isSelected={newWorkflowParam === wf.id}
                starred={starredWorkflowIds.includes(wf.id)}
                onToggleStar={() => toggleStarredWorkflow(wf.id)}
                onClick={() => handleWorkflowClick(wf.id)}
              />
            ))}

            {filteredWorkflows.length === 0 && (
              <div className="px-2 py-2 text-[12px] text-[var(--text-tertiary)]">
                No workflows match "{search}"
              </div>
            )}
          </>
        )}
      </ContentGate>
    </div>
  );
}
