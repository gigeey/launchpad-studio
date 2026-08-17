import { useEffect, useMemo, useState } from "react";
import { CalendarClock, Loader2, Pause, Play } from "lucide-react";
import { useChatStore } from "../../stores/chatStore";
import { assignmentDisplayLabel } from "../../hooks/useAssignments";
import { assignmentTriggerLabel } from "./assignmentAdapters";
import type { Assignment } from "../../types/api";

function formatNextFire(iso: string | null | undefined): string {
  if (!iso) return "Not scheduled";
  const d = new Date(iso);
  if (isNaN(d.getTime())) return "Invalid date";
  return d.toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "numeric",
    minute: "2-digit",
  });
}

/** Cron assignments carry a next-fire instant and sort ascending by it; every
 *  other trigger (no scheduled fire) sorts after them — same rule
 *  AssignmentList uses for the page-level list. */
function sortByNextFire(assignments: Assignment[]): Assignment[] {
  return [...assignments].sort((a, b) => {
    if (!a.next_fire_at && !b.next_fire_at) return 0;
    if (!a.next_fire_at) return 1;
    if (!b.next_fire_at) return -1;
    return new Date(a.next_fire_at).getTime() - new Date(b.next_fire_at).getTime();
  });
}

interface AssignmentAgentTilesProps {
  /** The agent whose assignments to list; null while no owner has been
   *  picked yet (the in-modal "no agent yet" create flow). */
  agentId: string | null;
  selectedAssignmentId: string | null;
  onSelect: (assignmentId: string) => void;
  onNew: () => void;
}

// A stable empty-array reference for the "no agent yet" / "nothing loaded
// yet" cases. Zustand's `useStore` is built on `useSyncExternalStore`, which
// requires a selector to return the same reference for an unchanged
// snapshot — returning a fresh `[]` literal on every call instead trips
// React's "getSnapshot should be cached" loop detection (surfaces as
// "Maximum update depth exceeded").
const EMPTY_ASSIGNMENTS: Assignment[] = [];

/** Left panel of the split-pane AssignmentEditorModal — every other
 *  assignment belonging to the same agent being edited, so switching between
 *  them (or starting a fresh one) never requires leaving the modal. Mirrors
 *  the pre-convergence ScheduledTaskModal's TaskListPanel.
 *
 *  Reads from chatStore's `assignmentsByAgent` cache (the same cache
 *  ChatHeader's Assignments badge reads) rather than fetching independently —
 *  the modal's own create/update/delete calls are routed through that same
 *  store so this list updates immediately after a save with no extra
 *  refetch, and never drifts from a second, parallel copy of the data. */
export function AssignmentAgentTiles({
  agentId,
  selectedAssignmentId,
  onSelect,
  onNew,
}: AssignmentAgentTilesProps) {
  const assignments = useChatStore((s) =>
    agentId ? s.assignmentsByAgent.get(agentId) ?? EMPTY_ASSIGNMENTS : EMPTY_ASSIGNMENTS,
  );
  const loadAssignments = useChatStore((s) => s.loadAssignments);
  const [loading, setLoading] = useState(!!agentId);

  useEffect(() => {
    if (!agentId) {
      setLoading(false);
      return;
    }
    setLoading(true);
    void loadAssignments(agentId).finally(() => setLoading(false));
  }, [agentId, loadAssignments]);

  const sorted = useMemo(() => sortByNextFire(assignments), [assignments]);

  return (
    <div className="w-[320px] flex-shrink-0 flex flex-col bg-[var(--modal-bg-primary)] border-r border-[var(--modal-border-secondary)]">
      <div className="px-4 h-[44px] flex items-center justify-between">
        <h2 className="text-[16px] font-bold text-[var(--modal-text-primary)] uppercase tracking-wider">
          Assignments
        </h2>
        {selectedAssignmentId && agentId && (
          <button
            type="button"
            onClick={onNew}
            data-testid="assignment-editor-tile-new"
            className="flex items-center justify-center gap-1 px-2 py-0.5 rounded-[6px] bg-[var(--success)] text-white text-[11px] font-semibold hover:brightness-110 transition-colors cursor-pointer"
          >
            New
          </button>
        )}
      </div>

      <div className="flex-1 overflow-y-auto px-0 py-0">
        {loading ? (
          <div className="flex items-center justify-center h-full">
            <Loader2 size={20} className="animate-spin text-[var(--modal-text-secondary)]" />
          </div>
        ) : !agentId ? (
          <div className="flex flex-col items-center justify-center h-full text-[var(--modal-text-secondary)] text-[13px] gap-1 px-4 text-center">
            <CalendarClock size={24} className="opacity-40 mb-1" />
            <p>Pick an agent to see its assignments</p>
          </div>
        ) : sorted.length === 0 ? (
          <div className="flex flex-col items-center justify-center h-full text-[var(--modal-text-secondary)] text-[13px] gap-1">
            <CalendarClock size={24} className="opacity-40 mb-1" />
            <p>No assignments yet</p>
          </div>
        ) : (
          <div className="flex flex-col gap-[8px] px-[10px] py-[10px]">
            {sorted.map((a) => {
              const isSelected = a.id === selectedAssignmentId;
              const nextLabel = formatNextFire(a.next_fire_at);
              const showNextFire = a.trigger.type === "Cron" && !!a.next_fire_at;
              return (
                <button
                  key={a.id}
                  type="button"
                  data-testid="assignment-editor-tile"
                  onClick={() => onSelect(a.id)}
                  className={`scheduled-list-tile w-full text-left rounded-[12px] border bg-[var(--modal-bg)] px-[12px] pt-[10px] pb-[6px] flex flex-col gap-[6px] transition-colors cursor-pointer ${
                    isSelected
                      ? "border-[var(--modal-accent)]"
                      : "border-[var(--modal-border-secondary)] hover:border-[var(--modal-border-primary)]"
                  }`}
                >
                  <div className="flex items-start gap-[8px] min-h-[34px]">
                    <div className="flex-shrink-0 mt-[2px]">
                      {a.enabled ? (
                        <Play size={11} className="text-[var(--success)]" />
                      ) : (
                        <Pause size={11} className="text-[var(--modal-text-tertiary)]" />
                      )}
                    </div>
                    <h4
                      className={`flex-1 text-[13px] font-semibold leading-[17px] line-clamp-2 break-words ${
                        a.enabled ? "text-[var(--modal-text-primary)]" : "text-[var(--modal-text-tertiary)]"
                      }`}
                    >
                      {assignmentDisplayLabel(a)}
                    </h4>
                  </div>

                  <div
                    className="flex items-center gap-[6px] text-[11px] text-[var(--modal-text-tertiary)] min-w-0 leading-[14px] border-t border-[var(--modal-border-secondary)] py-[8px] mt-[4px]"
                    title={showNextFire ? `Next: ${nextLabel}` : assignmentTriggerLabel(a)}
                  >
                    <CalendarClock className="w-[11px] h-[11px] flex-shrink-0" />
                    <span className="truncate">{assignmentTriggerLabel(a)}</span>
                    {showNextFire && (
                      <>
                        <span className="opacity-50">·</span>
                        <span className="truncate">{nextLabel}</span>
                      </>
                    )}
                  </div>
                </button>
              );
            })}
          </div>
        )}
      </div>
    </div>
  );
}
