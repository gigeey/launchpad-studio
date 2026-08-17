import { FolderOpen, User, Puzzle } from "lucide-react";
import type { WorkflowSummary, WorkflowSource } from "../../types/workflow";

const SOURCE_ICON: Record<WorkflowSource, typeof User> = {
  project: FolderOpen,
  user: User,
  plugin: Puzzle,
};

const SOURCE_LABEL: Record<WorkflowSource, string> = {
  project: "Project workflow",
  user: "User workflow",
  plugin: "Plugin workflow",
};

function formatDate(iso: string | null | undefined): string {
  if (!iso) return "—";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return "—";
  return d.toLocaleDateString(undefined, {
    month: "short",
    day: "numeric",
    year: "numeric",
  });
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

interface WorkflowTileProps {
  workflow: WorkflowSummary;
  enabled: boolean;
  onToggle: (enabled: boolean) => void;
  selectMode?: boolean;
}

export function WorkflowTile({
  workflow,
  enabled,
  onToggle,
  selectMode: _selectMode,
}: WorkflowTileProps) {
  const source: WorkflowSource = workflow.source ?? "user";
  const SourceIcon = SOURCE_ICON[source];
  const phaseCount = workflow.phase_count ?? 0;
  const phaseLabel = phaseCount > 0 ? `${phaseCount} phases` : "— phases";
  const description = workflow.description?.trim() || "—";

  return (
    <div className="competency-tile rounded-[14px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg)] px-[16px] py-[14px] flex flex-col gap-[8px]">
      <div className="flex items-start justify-between gap-[12px]">
        <h3 className="text-[15px] font-semibold text-[var(--modal-text-primary)] truncate">
          {workflow.name}
        </h3>
        <button
          type="button"
          onClick={() => onToggle(!enabled)}
          className={`relative w-[42px] h-[24px] rounded-full transition-colors cursor-pointer flex-shrink-0 ${
            enabled ? "bg-[var(--modal-accent)]" : "bg-[var(--modal-border-primary)]"
          }`}
          aria-label={enabled ? "Disable workflow" : "Enable workflow"}
        >
          <div
            className={`absolute top-[2px] w-[20px] h-[20px] rounded-full bg-white shadow transition-transform ${
              enabled ? "translate-x-[20px]" : "translate-x-[2px]"
            }`}
          />
        </button>
      </div>

      <p className="text-[13px] text-[var(--modal-text-secondary)] leading-[18px] line-clamp-2 min-h-[36px]">
        {description}
      </p>

      <div className="border-t border-[var(--modal-border-secondary)] -mx-[16px]" />

      <div className="flex items-center justify-between gap-[8px]">
        <div className="flex items-center gap-[8px] min-w-0 text-[12px] text-[var(--modal-text-tertiary)]">
          <span
            className="w-[20px] h-[20px] rounded-full border border-[var(--modal-border-secondary)] flex items-center justify-center flex-shrink-0"
            title={SOURCE_LABEL[source]}
          >
            <SourceIcon className="w-[11px] h-[11px]" />
          </span>
          <span className="truncate">Updated on {formatDate(workflow.updated_on)}</span>
          <span
            className="flex-shrink-0 px-[6px] py-[1px] rounded-full border border-[var(--modal-border-secondary)] text-[11px] text-[var(--modal-text-tertiary)]"
            title={phaseLabel}
          >
            {phaseLabel}
          </span>
        </div>
        <span className="flex-shrink-0 text-[12px] text-[var(--modal-text-tertiary)]">
          Last run {formatRelativeTime(workflow.last_run)}
        </span>
      </div>
    </div>
  );
}
