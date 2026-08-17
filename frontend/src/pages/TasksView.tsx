import { useEffect, useMemo, useRef, useState, useCallback } from "react";
import { createPortal } from "react-dom";
import { useNavigate, useParams } from "react-router-dom";
import { useWorkflowStore } from "../stores/workflowStore";
import { useUserPreferencesStore, useIsDark } from "../stores/userPreferencesStore";
import type { TaskSummary, TaskStatus, WorkflowSummary, WorkflowSource } from "../types/workflow";
import {
  Archive,
  ChevronDown,
  ChevronRight,
  Clock,
  Filter,
  FolderOpen,
  MoreVertical,
  Plus,
  Puzzle,
  Search,
  Star,
  Trash2,
  User,
  X,
} from "lucide-react";
import { AnimatePresence, motion } from "framer-motion";
import { useTaskCreateModalStore } from "../stores/taskCreateModalStore";
import workflowIcon from "../assets/workflowsNoBG.png";
import { ContentGate } from "../components/ContentGate";
import { BoardSkeleton, WorkflowTilesSkeleton } from "../components/shared/Skeletons";
import { useReadyLatch } from "../hooks/useReadyLatch";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function formatDate(iso: string): string {
  const d = new Date(iso);
  return d.toLocaleDateString(undefined, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

/** Date-only formatter for the Workflows catalog tiles (no time component — mirrors CompetenciesModal's WorkflowTile). */
function formatWorkflowDate(iso: string | null | undefined): string {
  if (!iso) return "—";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return "—";
  return d.toLocaleDateString(undefined, { month: "short", day: "numeric", year: "numeric" });
}

function formatRelativeTime(iso: string | null | undefined): string {
  if (!iso) return "—";
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return "—";
  const diffMs = Date.now() - date.getTime();
  const diffSec = Math.floor(diffMs / 1000);
  if (diffSec < 60) return "just now";
  const diffMin = Math.floor(diffSec / 60);
  if (diffMin < 60) return `${diffMin}m ago`;
  const diffHr = Math.floor(diffMin / 60);
  if (diffHr < 24) return `${diffHr}h ago`;
  const diffDay = Math.floor(diffHr / 24);
  if (diffDay < 30) return `${diffDay}d ago`;
  return date.toLocaleDateString([], { month: "short", day: "numeric" });
}

const WORKFLOW_SOURCE_ICON: Record<WorkflowSource, typeof User> = {
  project: FolderOpen,
  user: User,
  plugin: Puzzle,
};

const WORKFLOW_SOURCE_LABEL: Record<WorkflowSource, string> = {
  project: "Project workflow",
  user: "User workflow",
  plugin: "Plugin workflow",
};

// ---------------------------------------------------------------------------
// Column config
// ---------------------------------------------------------------------------

interface ColumnConfig {
  status: TaskStatus;
  title: string;
  dotColor: string;
}

const COLUMNS: ColumnConfig[] = [
  { status: "pending", title: "Pending", dotColor: "#9CA3AF" },
  { status: "running", title: "Running", dotColor: "#007AFF" },
  { status: "stopped", title: "Stopped", dotColor: "#F59E0B" },
  { status: "completed", title: "Completed", dotColor: "#007A5A" },
  { status: "failed", title: "Failed", dotColor: "#D32F2F" },
];

// ---------------------------------------------------------------------------
// TaskCard
// ---------------------------------------------------------------------------

function TaskCard({ task }: { task: TaskSummary }) {
  const { fetchTask, deleteTask, archiveTask } = useWorkflowStore();
  const navigate = useNavigate();
  const [menuOpen, setMenuOpen] = useState(false);
  const [confirmingDelete, setConfirmingDelete] = useState(false);
  const menuRef = useRef<HTMLDivElement>(null);
  const btnRef = useRef<HTMLButtonElement>(null);
  const [menuPos, setMenuPos] = useState<{ top: number; left: number } | null>(null);
  const progressPct =
    task.total_phases > 0
      ? (task.completed_phases / task.total_phases) * 100
      : 0;

  // Close menu on outside click
  useEffect(() => {
    if (!menuOpen) return;
    const handler = (e: MouseEvent) => {
      const target = e.target as Node;
      if (
        menuRef.current && !menuRef.current.contains(target) &&
        btnRef.current && !btnRef.current.contains(target)
      ) {
        setMenuOpen(false);
        setConfirmingDelete(false);
      }
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [menuOpen]);

  const toggleMenu = useCallback((e: React.MouseEvent) => {
    e.stopPropagation();
    if (!menuOpen && btnRef.current) {
      const rect = btnRef.current.getBoundingClientRect();
      setMenuPos({ top: rect.bottom + 4, left: rect.right });
    }
    setMenuOpen((o) => !o);
  }, [menuOpen]);

  const handleClick = () => {
    fetchTask(task.task_id);
    navigate(`/tasks/${task.task_id}/detail`);
  };

  const handleDelete = (e: React.MouseEvent) => {
    e.stopPropagation();
    setConfirmingDelete(true);
  };

  const handleConfirmDelete = (e: React.MouseEvent) => {
    e.stopPropagation();
    setMenuOpen(false);
    setConfirmingDelete(false);
    deleteTask(task.task_id);
  };

  const handleCancelDelete = (e: React.MouseEvent) => {
    e.stopPropagation();
    setConfirmingDelete(false);
  };

  const handleArchive = (e: React.MouseEvent) => {
    e.stopPropagation();
    setMenuOpen(false);
    archiveTask(task.task_id);
  };

  return (
    <motion.div
      layout
      layoutId={task.task_id}
      initial={{ opacity: 0, y: 8 }}
      animate={{ opacity: 1, y: 0 }}
      exit={{ opacity: 0, y: -8 }}
      transition={{ duration: 0.25 }}
      onClick={handleClick}
      className="group/card relative rounded-xl border border-[var(--border-secondary)] dark:border-[#333333] bg-[var(--bg-secondary)] p-3 cursor-pointer hover:border-[var(--accent)]/40 transition-colors shadow-xs"
    >
      {/* Three-dot menu */}
      <div className="absolute top-2 right-2">
        <button
          ref={btnRef}
          onClick={toggleMenu}
          className={`w-[24px] h-[24px] rounded-md flex items-center justify-center transition-all cursor-pointer ${menuOpen
            ? "opacity-100 bg-[var(--bg-hover)] text-[var(--text-primary)]"
            : "opacity-0 group-hover/card:opacity-100 text-[var(--text-secondary)] hover:bg-[var(--bg-hover)] hover:text-[var(--text-primary)]"
            }`}
          title="More options"
        >
          <MoreVertical size={14} />
        </button>
      </div>

      {menuOpen && menuPos && createPortal(
        <div
          ref={menuRef}
          className={`fixed ${confirmingDelete ? "w-[180px]" : "w-[140px]"} rounded-xl border border-[var(--border-primary)] bg-[var(--bg-secondary)] shadow-xl z-[9999] p-1 flex flex-col gap-[2px] transition-all`}
          style={{ top: menuPos.top, left: menuPos.left, transform: "translateX(-100%)" }}
          onClick={(e) => e.stopPropagation()}
        >
          <button
            onClick={handleArchive}
            className="w-full flex items-center gap-2.5 px-3 py-2 rounded-lg text-left transition-colors cursor-pointer text-[var(--text-primary)] hover:bg-[var(--bg-hover)]"
          >
            <Archive size={14} className="flex-shrink-0 text-[var(--text-secondary)]" />
            <span className="text-[13px] font-medium">Archive</span>
          </button>
          {confirmingDelete ? (
            <div className="flex flex-col gap-1 px-2 py-1.5">
              <span className="text-[12px] text-[var(--text-secondary)] px-1">Delete this task?</span>
              <div className="flex gap-1">
                <button
                  onClick={handleCancelDelete}
                  className="flex-1 px-2 py-1.5 rounded-lg text-[12px] font-medium text-[var(--text-primary)] hover:bg-[var(--bg-hover)] transition-colors cursor-pointer"
                >
                  Cancel
                </button>
                <button
                  onClick={handleConfirmDelete}
                  className="flex-1 px-2 py-1.5 rounded-lg text-[12px] font-medium text-white bg-[#E01E5A] hover:bg-[#c4174d] transition-colors cursor-pointer"
                >
                  Delete
                </button>
              </div>
            </div>
          ) : (
            <button
              onClick={handleDelete}
              className="w-full flex items-center gap-2.5 px-3 py-2 rounded-lg text-left transition-colors cursor-pointer text-[#E01E5A] hover:bg-[#E01E5A] hover:text-white"
            >
              <Trash2 size={14} className="flex-shrink-0" />
              <span className="text-[13px] font-medium">Delete</span>
            </button>
          )}
        </div>,
        document.body
      )}

      <div className="text-[14px] font-bold text-[var(--text-primary)] truncate pr-6">
        {task.project_name}
      </div>

      {/* Progress bar */}
      <div className="mt-2 h-1 rounded-full bg-[var(--bg-tertiary)] overflow-hidden">
        <div
          className="h-full rounded-full transition-all duration-300"
          style={{
            width: `${progressPct}%`,
            backgroundColor:
              task.status === "failed"
                ? "#E01E59"
                : task.status === "completed"
                  ? "#2EB57D"
                  : "#36C4F0",
          }}
        />
      </div>

      <div className="flex items-center justify-between mt-1.5">
        <span className="text-[12px] text-[var(--text-secondary)]">
          {formatDate(task.created)}
        </span>
        <span className="text-[11px] text-[var(--text-secondary)] tabular-nums">
          {task.completed_phases}/{task.total_phases}
        </span>
      </div>

      {task.status === "running" && (
        <div className="text-[11px] text-[var(--accent)] mt-1 truncate">
          Phase {task.completed_phases + 1} of {task.total_phases}
        </div>
      )}
    </motion.div>
  );
}

// ---------------------------------------------------------------------------
// SwimlaneColumn
// ---------------------------------------------------------------------------

function SwimlaneColumn({
  config,
  tasks,
}: {
  config: ColumnConfig;
  tasks: TaskSummary[];
}) {
  const isDark = useIsDark();
  // Sort newest first
  const sorted = useMemo(
    () => [...tasks].sort((a, b) => new Date(b.created).getTime() - new Date(a.created).getTime()),
    [tasks],
  );

  return (
    <div
      className="w-[280px] min-w-[280px] shrink-0 flex flex-col gap-2 min-h-[60px] rounded-xl p-3 border border-[var(--border-secondary)] hover:border-[var(--border-primary)] transition-colors"
      style={{ backgroundColor: isDark ? `color-mix(in srgb, ${config.dotColor} 6%, var(--bg-secondary))` : '#F7F8F9' }}
    >
      {/* Column header */}
      <div className="flex items-center gap-1.5 px-1 mb-1">
        <span
          className="w-2 h-2 rounded-full flex-shrink-0"
          style={{ backgroundColor: config.dotColor }}
        />
        <span className="text-[13px] font-semibold text-[var(--text-secondary)]">
          {config.title}
        </span>
        <span className="text-[11px] text-[var(--text-tertiary)] bg-[var(--bg-secondary)] px-1.5 py-0.5 rounded-full tabular-nums ml-auto">
          {tasks.length}
        </span>
      </div>
      <AnimatePresence mode="popLayout">
        {sorted.map((t) => (
          <TaskCard key={t.task_id} task={t} />
        ))}
      </AnimatePresence>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Swimlane (one per workflow)
// ---------------------------------------------------------------------------

interface SwimlaneData {
  workflowId: string;
  workflowName: string;
  tasks: TaskSummary[];
}

function Swimlane({ data, columns }: { data: SwimlaneData; columns: ColumnConfig[] }) {
  const [collapsed, setCollapsed] = useState(false);
  const openModal = useTaskCreateModalStore((s) => s.open);

  const byStatus = useMemo(() => {
    const map: Record<TaskStatus, TaskSummary[]> = {
      pending: [],
      running: [],
      completed: [],
      failed: [],
      archived: [],
      stopped: [],
    };
    for (const t of data.tasks) {
      map[t.status]?.push(t);
    }
    return map;
  }, [data.tasks]);

  return (
    <div className="flex flex-col mb-4">
      {/* Swimlane header */}
      <div className="group/lane flex items-center gap-2 px-4 py-2.5 mb-1">
        <button
          onClick={() => setCollapsed(!collapsed)}
          className="flex items-center gap-2 hover:bg-[var(--bg-secondary)]/50 rounded-lg transition-colors"
        >
          {collapsed ? (
            <ChevronRight size={14} className="text-[var(--text-secondary)] flex-shrink-0" />
          ) : (
            <ChevronDown size={14} className="text-[var(--text-secondary)] flex-shrink-0" />
          )}
          <span className="text-[13px] font-semibold text-[var(--text-primary)]">
            {data.workflowName}
          </span>
        </button>
        <div className="relative">
          <span className="text-[11px] text-[var(--text-secondary)] bg-[var(--bg-secondary)] rounded-full px-2 py-0.5 tabular-nums group-hover/lane:opacity-0 transition-opacity duration-150">
            {data.tasks.length}
          </span>
          <button
            onClick={() => openModal(data.workflowId)}
            className="absolute inset-0 flex items-center justify-center opacity-0 scale-75 group-hover/lane:opacity-100 group-hover/lane:scale-100 transition-all duration-150"
            title="New task"
          >
            <span className="flex items-center justify-center w-[22px] h-[22px] rounded-md bg-[var(--bg-tertiary)] text-[var(--text-secondary)] cursor-pointer hover:bg-[var(--border-secondary)] hover:text-[var(--text-primary)] transition-colors border border-[var(--border-secondary)]">
              <Plus size={14} />
            </span>
          </button>
        </div>
      </div>

      {/* Columns */}
      {!collapsed && (
        <div className="flex gap-4 px-4 pb-4 pt-1">
          {columns.map((col) => (
            <SwimlaneColumn
              key={col.status}
              config={col}
              tasks={byStatus[col.status]}
            />
          ))}
        </div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// WorkflowGridTile (Workflows catalog view)
// ---------------------------------------------------------------------------

/**
 * Card for the Tasks → Workflows catalog grid. Visual language mirrors
 * CompetenciesModal's WorkflowTile (rounded card, truncated title, 2-line
 * description, divider, source + updated/phase-count footer) but swaps the
 * enable toggle — meaningless outside the agent-competency context — for a
 * "new task" action, since browsing this catalog is about starting work,
 * not enabling/disabling a workflow.
 */
function WorkflowGridTile({
  workflow,
  starred,
  onToggleStar,
  onStart,
}: {
  workflow: WorkflowSummary;
  starred: boolean;
  onToggleStar: () => void;
  onStart: () => void;
}) {
  const source: WorkflowSource = workflow.source ?? "user";
  const SourceIcon = WORKFLOW_SOURCE_ICON[source];
  const phaseCount = workflow.phase_count ?? 0;
  const phaseLabel = phaseCount > 0 ? `${phaseCount} phases` : "— phases";
  const description = workflow.description?.trim() || "No description";

  const handleToggleStar = (e: React.MouseEvent | React.KeyboardEvent) => {
    e.stopPropagation();
    onToggleStar();
  };

  return (
    <div
      role="button"
      tabIndex={0}
      onClick={onStart}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          onStart();
        }
      }}
      className="group/wftile rounded-xl border border-[var(--border-secondary)] dark:border-[#333333] bg-[var(--bg-secondary)] px-4 py-[14px] flex flex-col gap-2 cursor-pointer hover:border-[var(--accent)]/40 transition-colors shadow-xs"
    >
      <div className="flex items-start justify-between gap-3">
        <h3 className="text-[15px] font-semibold text-[var(--text-primary)] truncate">
          {workflow.name}
        </h3>
        <div className="flex-shrink-0 flex items-center gap-1.5">
          <button
            type="button"
            onClick={handleToggleStar}
            onKeyDown={(e) => {
              if (e.key === "Enter" || e.key === " ") handleToggleStar(e);
            }}
            title={starred ? "Unstar workflow" : "Star workflow"}
            aria-label={starred ? "Unstar workflow" : "Star workflow"}
            className={`flex items-center justify-center w-[26px] h-[26px] rounded-md border transition-colors ${starred
              ? "border-[var(--border-secondary)] text-[var(--accent)]"
              : "border-[var(--border-secondary)] text-[var(--text-tertiary)] hover:text-[var(--text-primary)] hover:bg-[var(--bg-tertiary)]"
              }`}
          >
            <Star size={14} fill={starred ? "currentColor" : "none"} />
          </button>
          <span
            title="New task"
            className="flex items-center justify-center w-[26px] h-[26px] rounded-md bg-[var(--bg-tertiary)] text-[var(--text-secondary)] group-hover/wftile:bg-[var(--accent)] group-hover/wftile:text-white transition-colors border border-[var(--border-secondary)]"
          >
            <Plus size={14} />
          </span>
        </div>
      </div>

      <p className="text-[13px] text-[var(--text-secondary)] leading-[18px] line-clamp-2 min-h-[36px]">
        {description}
      </p>

      <div className="border-t border-[var(--border-secondary)] -mx-4" />

      <div className="flex items-center justify-between gap-2">
        <div className="flex items-center gap-2 min-w-0 text-[12px] text-[var(--text-tertiary)]">
          <span
            className="w-5 h-5 rounded-full border border-[var(--border-secondary)] flex items-center justify-center flex-shrink-0"
            title={WORKFLOW_SOURCE_LABEL[source]}
          >
            <SourceIcon className="w-[11px] h-[11px]" />
          </span>
          <span className="truncate">Updated {formatWorkflowDate(workflow.updated_on)}</span>
          <span
            className="flex-shrink-0 px-[6px] py-[1px] rounded-full border border-[var(--border-secondary)] text-[11px] text-[var(--text-tertiary)]"
            title={phaseLabel}
          >
            {phaseLabel}
          </span>
        </div>
        <span className="flex-shrink-0 text-[12px] text-[var(--text-tertiary)]">
          Last run {formatRelativeTime(workflow.last_run)}
        </span>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// WorkflowSearchBar (Workflows catalog view)
// ---------------------------------------------------------------------------

/** Full-width filter-as-you-type search bar for the Workflows catalog grid. */
function WorkflowSearchBar({
  value,
  onChange,
}: {
  value: string;
  onChange: (value: string) => void;
}) {
  return (
    <div className="mb-4 flex items-center gap-2.5 h-[44px] px-4 rounded-2xl border border-[var(--border-secondary)] bg-[var(--bg-secondary)] text-[var(--text-secondary)] focus-within:border-[var(--input-focus-border)] transition-colors">
      <Search size={18} className="flex-shrink-0" />
      <input
        type="text"
        value={value}
        onChange={(e) => onChange(e.target.value)}
        placeholder="Search workflows..."
        className="flex-1 bg-transparent text-[14px] text-[var(--text-primary)] placeholder:text-[var(--text-tertiary)] outline-none border-none"
      />
      {value && (
        <button
          type="button"
          onClick={() => onChange("")}
          className="flex-shrink-0 hover:text-[var(--text-primary)] transition-colors cursor-pointer"
        >
          <X size={16} />
        </button>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// WorkflowSection (Starred / Recent / All headers on the Workflows catalog)
// ---------------------------------------------------------------------------

function WorkflowSection({
  icon: Icon,
  title,
  workflows,
  starredWorkflowIds,
  onToggleStar,
  onStart,
}: {
  icon: typeof Star;
  title: string;
  workflows: WorkflowSummary[];
  starredWorkflowIds: string[];
  onToggleStar: (id: string) => void;
  onStart: (id: string) => void;
}) {
  return (
    <div className="mb-5">
      <h2 className="flex items-center gap-1.5 text-[12px] font-semibold text-[var(--text-tertiary)] uppercase tracking-wider mb-2">
        <Icon size={12} />
        {title}
      </h2>
      <div className="grid grid-cols-1 @xl:grid-cols-2 @5xl:grid-cols-3 gap-4">
        {workflows.map((wf) => (
          <WorkflowGridTile
            key={wf.id}
            workflow={wf}
            starred={starredWorkflowIds.includes(wf.id)}
            onToggleStar={() => onToggleStar(wf.id)}
            onStart={() => onStart(wf.id)}
          />
        ))}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Filter Popover
// ---------------------------------------------------------------------------

const MAX_VISIBLE_WORKFLOWS = 20;
/** Cap on the "Recent" quick-access row — a glanceable strip, not a duplicate of the full catalog. */
const MAX_RECENT_WORKFLOWS = 6;

function FilterPopover({
  allWorkflows,
}: {
  allWorkflows: string[];
}) {
  const [open, setOpen] = useState(false);
  const [wfSearch, setWfSearch] = useState("");
  const ref = useRef<HTMLDivElement>(null);

  const statusFilters = useUserPreferencesStore((s) => s.kanbanStatusFilters);
  const setStatusFilters = useUserPreferencesStore((s) => s.setKanbanStatusFilters);
  const workflowFilters = useUserPreferencesStore((s) => s.kanbanWorkflowFilters);
  const setWorkflowFilters = useUserPreferencesStore((s) => s.setKanbanWorkflowFilters);

  const activeCount = statusFilters.length + workflowFilters.length;

  useEffect(() => {
    if (!open) return;
    const handler = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [open]);

  const toggleStatus = (status: string) => {
    setStatusFilters(
      statusFilters.includes(status)
        ? statusFilters.filter((s) => s !== status)
        : [...statusFilters, status],
    );
  };

  const toggleWorkflow = (wf: string) => {
    setWorkflowFilters(
      workflowFilters.includes(wf)
        ? workflowFilters.filter((w) => w !== wf)
        : [...workflowFilters, wf],
    );
  };

  const handleReset = () => {
    setStatusFilters([]);
    setWorkflowFilters([]);
    setOpen(false);
  };

  const filteredWfs = wfSearch.trim()
    ? allWorkflows.filter((wf) => wf.toLowerCase().includes(wfSearch.toLowerCase()))
    : allWorkflows;
  const visibleWfs = filteredWfs.slice(0, MAX_VISIBLE_WORKFLOWS);

  return (
    <div className="relative" ref={ref}>
      <button
        onClick={() => setOpen((o) => !o)}
        className={`flex items-center h-[32px] text-[13px] font-medium cursor-pointer rounded-lg overflow-hidden transition-colors duration-200 ${activeCount > 0
          ? "bg-[#1a1a2e] dark:bg-[#2a2a3e] dark:border dark:border-[#444] text-white"
          : "border border-[#3E3F3F]/25 dark:border-[#555] text-[#3E3F3F] dark:text-[var(--text-secondary)] hover:bg-[var(--bg-hover)]"
          }`}
      >
        <motion.span
          initial={false}
          animate={{ width: activeCount > 0 ? 28 : 0, opacity: activeCount > 0 ? 1 : 0 }}
          transition={{ duration: 0.2, ease: "easeInOut" }}
          className="flex items-center justify-center h-full bg-white/20 dark:bg-[#1a1a2e]/10 text-[11px] font-bold overflow-hidden flex-shrink-0"
        >
          {activeCount}
        </motion.span>
        <span className="flex items-center gap-1.5 px-3">
          <Filter size={13} />
          Filter
        </span>
      </button>

      <AnimatePresence>
        {open && (
          <motion.div
            initial={{ opacity: 0, scale: 0.95, y: 4 }}
            animate={{ opacity: 1, scale: 1, y: 0 }}
            exit={{ opacity: 0, scale: 0.95, y: 4 }}
            transition={{ duration: 0.12, ease: "easeOut" }}
            className="absolute right-0 top-full mt-1 w-[280px] rounded-xl border border-gray-200 dark:border-[var(--border-primary)] bg-white dark:bg-[var(--bg-primary)] shadow-xl z-50 p-3 flex flex-col gap-3 origin-top-right">
            {/* Status filters — equal width grid */}
            <div>
              <div className="text-[10px] font-semibold text-[var(--text-tertiary)] uppercase tracking-wider mb-1.5">
                Status
              </div>
              <div className="grid grid-cols-2 gap-1.5">
                {COLUMNS.map((col) => {
                  const active = statusFilters.includes(col.status);
                  // Use same colors as progress bars
                  const colorMap: Record<string, string> = {
                    pending: "#9CA3AF",
                    running: "#36C4F0",
                    completed: "#2EB57D",
                    failed: "#E01E59",
                  };
                  const color = colorMap[col.status] ?? col.dotColor;
                  return (
                    <button
                      key={col.status}
                      onClick={() => toggleStatus(col.status)}
                      className={`py-[6px] rounded-md text-[13px] font-medium transition-colors cursor-pointer text-center leading-none ${active
                        ? "text-white border border-transparent"
                        : "bg-gray-50 dark:bg-[var(--bg-secondary)] text-[var(--text-primary)] hover:bg-gray-100 dark:hover:bg-[var(--bg-hover)] border border-gray-300 dark:border-[var(--border-secondary)]"
                        }`}
                      style={active ? { backgroundColor: color } : undefined}
                    >
                      {col.title}
                    </button>
                  );
                })}
              </div>
            </div>

            {/* Workflow filters */}
            {allWorkflows.length > 0 && (
              <div>
                <div className="text-[10px] font-semibold text-[var(--text-tertiary)] uppercase tracking-wider mb-1.5">
                  Workflow
                </div>

                {/* Search */}
                {allWorkflows.length > 5 && (
                  <div className="mb-1.5">
                    <input
                      type="text"
                      value={wfSearch}
                      onChange={(e) => setWfSearch(e.target.value)}
                      placeholder="Search workflows..."
                      className="w-full px-2.5 py-[5px] text-[12px] rounded-md border border-[var(--border-secondary)] bg-[var(--bg-primary)] text-[var(--text-primary)] placeholder:text-[var(--text-tertiary)] focus:outline-none focus:border-[var(--input-focus-border)] transition-colors"
                    />
                  </div>
                )}

                <div className="flex flex-col gap-0.5 max-h-[200px] overflow-y-auto custom-scrollbar">
                  {visibleWfs.map((wf) => {
                    const active = workflowFilters.includes(wf);
                    return (
                      <button
                        key={wf}
                        onClick={() => toggleWorkflow(wf)}
                        className={`flex items-center gap-2.5 px-2.5 py-[6px] rounded-md text-[13px] text-left transition-colors cursor-pointer ${active
                          ? "bg-[var(--bg-hover)] text-[var(--text-primary)] font-medium"
                          : "text-[var(--text-primary)] hover:bg-[var(--bg-hover)]"
                          }`}
                      >
                        <div
                          className={`w-4 h-4 rounded flex items-center justify-center flex-shrink-0 transition-colors ${active
                            ? "bg-[var(--accent)] border-[var(--accent)]"
                            : "border border-gray-300 dark:border-[var(--border-secondary)] bg-transparent"
                            }`}
                        >
                          {active && (
                            <svg width="10" height="10" viewBox="0 0 10 10" fill="none">
                              <path d="M2 5L4 7L8 3" stroke="white" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
                            </svg>
                          )}
                        </div>
                        <span className="truncate">{wf}</span>
                      </button>
                    );
                  })}
                  {filteredWfs.length > MAX_VISIBLE_WORKFLOWS && (
                    <div className="px-2.5 py-1.5 text-[11px] text-[var(--text-tertiary)]">
                      +{filteredWfs.length - MAX_VISIBLE_WORKFLOWS} more — refine search
                    </div>
                  )}
                  {filteredWfs.length === 0 && wfSearch.trim() && (
                    <div className="px-2.5 py-1.5 text-[12px] text-[var(--text-tertiary)]">
                      No match
                    </div>
                  )}
                </div>
              </div>
            )}

            {/* Reset */}
            {activeCount > 0 && (
              <button
                onClick={handleReset}
                className="w-full py-[7px] rounded-md text-[13px] font-medium text-center text-[var(--text-primary)] bg-gray-100 dark:bg-[var(--bg-secondary)] hover:bg-gray-200 dark:hover:bg-[var(--bg-hover)] transition-colors cursor-pointer"
              >
                Reset filters
              </button>
            )}
          </motion.div>
        )}
      </AnimatePresence>
    </div>
  );
}

// ---------------------------------------------------------------------------
// TasksView (Kanban Board with Workflow Swimlanes)
// ---------------------------------------------------------------------------

/** Archived swimlane columns — show all archived tasks in a single "Archived" column per workflow. */
const ARCHIVED_COLUMNS: ColumnConfig[] = [
  { status: "archived", title: "Archived", dotColor: "#9CA3AF" },
];

export function TasksView() {
  const { subMenuSlug } = useParams<{ subMenuSlug: string }>();
  const isArchived = subMenuSlug === "archived";
  const isWorkflowsCatalog = subMenuSlug === "workflows";

  const {
    tasks,
    archivedTasks,
    workflows,
    loading,
    error,
    fetchTasks,
    fetchArchivedTasks,
    fetchWorkflows,
  } = useWorkflowStore();
  const statusFilters = useUserPreferencesStore((s) => s.kanbanStatusFilters);
  const workflowFilters = useUserPreferencesStore((s) => s.kanbanWorkflowFilters);
  const starredWorkflowIds = useUserPreferencesStore((s) => s.starredWorkflowIds);
  const toggleStarredWorkflow = useUserPreferencesStore((s) => s.toggleStarredWorkflow);
  const openTaskCreateModal = useTaskCreateModalStore((s) => s.open);
  const [workflowSearch, setWorkflowSearch] = useState("");

  const activeTasks = isArchived ? archivedTasks : tasks;

  const hasRunning = !isArchived && !isWorkflowsCatalog && tasks.some((t) => t.status === "running");

  // Re-arms whenever the active sub-view swaps its underlying data source
  // (tasks vs archivedTasks vs the workflow catalog) without unmounting.
  const ready = useReadyLatch(
    isWorkflowsCatalog ? workflows.length > 0 : activeTasks.length > 0,
    loading,
    isArchived ? "archived" : isWorkflowsCatalog ? "workflows" : "active",
  );

  useEffect(() => {
    if (isArchived) {
      fetchArchivedTasks();
    } else if (isWorkflowsCatalog) {
      fetchWorkflows();
    } else {
      fetchTasks();
    }
    // Re-fetch when this view becomes visible (e.g. navigating back from detail)
    const onVisibility = () => {
      if (!document.hidden) {
        if (isArchived) fetchArchivedTasks();
        else if (isWorkflowsCatalog) fetchWorkflows();
        else fetchTasks();
      }
    };
    document.addEventListener("visibilitychange", onVisibility);
    return () => document.removeEventListener("visibilitychange", onVisibility);
  }, [fetchTasks, fetchArchivedTasks, fetchWorkflows, isArchived, isWorkflowsCatalog]);

  // Light poll when there are running tasks (no SSE on this view)
  useEffect(() => {
    if (!hasRunning) return;
    const id = setInterval(fetchTasks, 3000);
    return () => clearInterval(id);
  }, [hasRunning, fetchTasks]);

  // All unique workflow IDs for the filter popover
  const allWorkflows = useMemo(
    () => [...new Set(activeTasks.map((t) => t.workflow))].sort(),
    [activeTasks],
  );

  // Workflow catalog, sorted for the Workflows tile view
  const sortedCatalogWorkflows = useMemo(
    () => [...workflows].sort((a, b) => a.name.localeCompare(b.name)),
    [workflows],
  );

  // Filtered by the catalog search bar (name + description, case-insensitive)
  const filteredCatalogWorkflows = useMemo(() => {
    const q = workflowSearch.trim().toLowerCase();
    if (!q) return sortedCatalogWorkflows;
    return sortedCatalogWorkflows.filter(
      (wf) =>
        wf.name.toLowerCase().includes(q) ||
        (wf.description ?? "").toLowerCase().includes(q),
    );
  }, [sortedCatalogWorkflows, workflowSearch]);

  // Quick-access rows for the browse (non-search) state — only shown while
  // workflowSearch is empty, so they don't duplicate search results.
  const isBrowsingCatalog = isWorkflowsCatalog && !workflowSearch.trim();

  const starredCatalogWorkflows = useMemo(
    () => sortedCatalogWorkflows.filter((wf) => starredWorkflowIds.includes(wf.id)),
    [sortedCatalogWorkflows, starredWorkflowIds],
  );

  // Most-recently-run workflows (backend-computed `last_run`, the latest
  // task creation time for that workflow), capped so this stays a glanceable
  // strip rather than a second copy of the full catalog.
  const recentCatalogWorkflows = useMemo(() => {
    return [...workflows]
      .filter((wf) => !!wf.last_run)
      .sort((a, b) => new Date(b.last_run!).getTime() - new Date(a.last_run!).getTime())
      .slice(0, MAX_RECENT_WORKFLOWS);
  }, [workflows]);

  const hasQuickAccessRows =
    isBrowsingCatalog && (starredCatalogWorkflows.length > 0 || recentCatalogWorkflows.length > 0);

  // Filtered columns config
  const visibleColumns = useMemo(() => {
    if (isArchived) return ARCHIVED_COLUMNS;
    return statusFilters.length > 0
      ? COLUMNS.filter((c) => statusFilters.includes(c.status))
      : COLUMNS;
  }, [statusFilters, isArchived]);

  const swimlanes = useMemo(() => {
    let filtered = workflowFilters.length > 0
      ? activeTasks.filter((t) => workflowFilters.includes(t.workflow))
      : activeTasks;

    if (!isArchived && statusFilters.length > 0) {
      filtered = filtered.filter((t) => statusFilters.includes(t.status));
    }

    const map = new Map<string, SwimlaneData>();
    for (const t of filtered) {
      let lane = map.get(t.workflow);
      if (!lane) {
        lane = { workflowId: t.workflow, workflowName: t.workflow, tasks: [] };
        map.set(t.workflow, lane);
      }
      lane.tasks.push(t);
    }

    return Array.from(map.values()).sort((a, b) =>
      a.workflowName.localeCompare(b.workflowName),
    );
  }, [activeTasks, workflowFilters, statusFilters, isArchived]);

  return (
    <div className="flex flex-1 flex-col min-h-0 p-6 overflow-y-auto">
      <div className="mb-4 flex items-start justify-between">
        <div>
          <h1 className="text-[22px] font-bold text-[var(--text-primary)] mb-1">
            {isArchived ? "Archived Tasks" : isWorkflowsCatalog ? "Workflows" : "Workflow Tasks"}
          </h1>
          <p className="text-[13px] text-[var(--text-secondary)]">
            {isArchived
              ? "Previously archived workflow tasks"
              : isWorkflowsCatalog
                ? "Browse available workflows and start a new task"
                : "View and track all workflow tasks"}
          </p>
        </div>
        {!isArchived && !isWorkflowsCatalog && <FilterPopover allWorkflows={allWorkflows} />}
      </div>

      <ContentGate
        ready={ready}
        skeleton={isWorkflowsCatalog ? <WorkflowTilesSkeleton /> : <BoardSkeleton />}
        className="flex-1 min-h-0 flex flex-col"
      >
        {error && (
          <div className="rounded-md bg-[#E01E5A] px-3 py-1.5 text-[13px] font-bold text-white mb-4">
            {error}
          </div>
        )}

        {!error && isWorkflowsCatalog && sortedCatalogWorkflows.length === 0 && (
          <div className="flex flex-col items-center justify-center py-16 gap-3 text-center">
            <img src={workflowIcon} alt="" className="w-[426px] h-[426px] object-contain select-none" draggable={false} />
          </div>
        )}

        {!error && isWorkflowsCatalog && sortedCatalogWorkflows.length > 0 && (
          <div className="@container flex-1 min-h-0 flex flex-col">
            <WorkflowSearchBar value={workflowSearch} onChange={setWorkflowSearch} />
            {filteredCatalogWorkflows.length > 0 ? (
              <div className="flex-1 min-h-0 overflow-y-auto pb-4">
                {isBrowsingCatalog && starredCatalogWorkflows.length > 0 && (
                  <WorkflowSection
                    icon={Star}
                    title="Starred"
                    workflows={starredCatalogWorkflows}
                    starredWorkflowIds={starredWorkflowIds}
                    onToggleStar={toggleStarredWorkflow}
                    onStart={openTaskCreateModal}
                  />
                )}
                {isBrowsingCatalog && recentCatalogWorkflows.length > 0 && (
                  <WorkflowSection
                    icon={Clock}
                    title="Recent"
                    workflows={recentCatalogWorkflows}
                    starredWorkflowIds={starredWorkflowIds}
                    onToggleStar={toggleStarredWorkflow}
                    onStart={openTaskCreateModal}
                  />
                )}
                {hasQuickAccessRows && (
                  <h2 className="text-[12px] font-semibold text-[var(--text-tertiary)] uppercase tracking-wider mb-2">
                    All workflows
                  </h2>
                )}
                <div className="grid grid-cols-1 @xl:grid-cols-2 @5xl:grid-cols-3 gap-4">
                  {filteredCatalogWorkflows.map((wf) => (
                    <WorkflowGridTile
                      key={wf.id}
                      workflow={wf}
                      starred={starredWorkflowIds.includes(wf.id)}
                      onToggleStar={() => toggleStarredWorkflow(wf.id)}
                      onStart={() => openTaskCreateModal(wf.id)}
                    />
                  ))}
                </div>
              </div>
            ) : (
              <div className="flex flex-col items-center justify-center py-16 text-center">
                <p className="text-[13px] text-[var(--text-secondary)]">
                  No workflows match "{workflowSearch}"
                </p>
              </div>
            )}
          </div>
        )}

        {!error && !isWorkflowsCatalog && swimlanes.length === 0 && (
          <div className="flex flex-col items-center justify-center py-16 gap-3 text-center">
            {isArchived ? (
              <>
                <div className="w-14 h-14 rounded-2xl bg-[var(--bg-tertiary)] flex items-center justify-center border border-[var(--border-secondary)]">
                  <Archive size={24} className="text-[var(--text-secondary)]" />
                </div>
                <p className="text-[14px] text-[var(--text-secondary)]">No archived tasks</p>
                <p className="text-[12px] text-[var(--text-tertiary)] max-w-[300px]">
                  Tasks you archive will appear here
                </p>
              </>
            ) : (
              <>
                <img src={workflowIcon} alt="" className="w-[426px] h-[426px] object-contain select-none" draggable={false} />
              </>
            )}
          </div>
        )}

        {!isWorkflowsCatalog && swimlanes.length > 0 && (
          <div className="flex flex-col min-h-0 overflow-x-auto pb-4">
            <div className="min-w-max flex flex-col gap-2">
              {/* Swimlanes */}
              {swimlanes.map((lane) => (
                <Swimlane key={lane.workflowId} data={lane} columns={visibleColumns} />
              ))}
            </div>
          </div>
        )}
      </ContentGate>
    </div>
  );
}
