import { useMemo } from "react";
import { CalendarDays, Clock, ListChecks, Plus } from "lucide-react";
import { useUserPreferencesStore } from "../../stores/userPreferencesStore";
import { useAssignmentEditorModalStore } from "../../stores/assignmentEditorModalStore";
import { useAssignments, assignmentDisplayLabel, type AssignmentWithOwner } from "../../hooks/useAssignments";
import { ScheduledTaskOwnerChip } from "../scheduled/ScheduledTaskOwnerChip";

// ---------------------------------------------------------------------------
// Body of the collapsible "Assignments" sub-menu column. AppShell renders the
// "Assignments" title above this; here we render the Calendar | List view
// switcher followed by the "new assignment" entry point.
//
// The Calendar/List selection reuses the shared `scheduledView` preference so
// the toggle state is consistent with the (now-superseded) Scheduled surface
// during the convergence — no new persisted field is needed.
//
// The "New assignment" button opens the trigger-aware create modal (mounted
// once in AssignmentsView) with no agent preselected.
// ---------------------------------------------------------------------------

/** True if `iso` falls on today's calendar date in local time. */
function isToday(iso: string): boolean {
  const d = new Date(iso);
  const now = new Date();
  return (
    d.getFullYear() === now.getFullYear() &&
    d.getMonth() === now.getMonth() &&
    d.getDate() === now.getDate()
  );
}

/** Only cron-triggered assignments carry a `next_fire_at`; the others never
 *  qualify for "firing today" regardless of trigger type. */
function assignmentsFiringToday(assignments: AssignmentWithOwner[]): AssignmentWithOwner[] {
  return assignments
    .filter((a) => a.trigger.type === "Cron" && !!a.next_fire_at && isToday(a.next_fire_at))
    .sort((a, b) => new Date(a.next_fire_at!).getTime() - new Date(b.next_fire_at!).getTime());
}

function formatFireTime(iso: string): string {
  return new Date(iso).toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" });
}

/** A single row in the stacked Calendar / List view switcher — full-width and
 *  left-aligned, matching the other sidebar rows. */
function ViewToggleButton({
  active,
  onClick,
  icon: Icon,
  label,
}: {
  active: boolean;
  onClick: () => void;
  icon: typeof CalendarDays;
  label: string;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      aria-pressed={active}
      className={`mx-[4px] flex items-center gap-[6px] px-[8px] py-[2px] rounded-[6px] text-[15px] font-medium transition-colors cursor-pointer ${active
        ? "bg-[var(--sidebar-active-bg)] text-[var(--sidebar-active-text-primary)]"
        : "text-[var(--text-secondary)] hover:text-[var(--sidebar-text-primary,var(--text-primary))] hover:bg-[var(--bg-hover)]"
        }`}
    >
      <Icon size={15} />
      {label}
    </button>
  );
}

/** Compact row for an assignment firing later today — fire time, name, and
 *  owning agent, opening straight into the editor on click. */
function TodayAssignmentRow({ assignment }: { assignment: AssignmentWithOwner }) {
  return (
    <button
      type="button"
      onClick={() => useAssignmentEditorModalStore.getState().openEdit(assignment.owner.id, assignment.id)}
      className="mx-[4px] flex flex-col gap-[4px] px-[8px] py-[6px] rounded-[8px] text-left hover:bg-[var(--bg-hover)] transition-colors cursor-pointer"
    >
      <div className="flex items-center gap-[6px] min-w-0">
        <Clock size={12} className="shrink-0 text-[var(--text-tertiary)]" />
        <span className="shrink-0 text-[12px] font-medium text-[var(--text-tertiary)]">
          {formatFireTime(assignment.next_fire_at!)}
        </span>
        <span className="text-[13px] font-medium text-[var(--text-primary)] truncate">
          {assignmentDisplayLabel(assignment)}
        </span>
      </div>
      <ScheduledTaskOwnerChip owner={assignment.owner} size="sm" />
    </button>
  );
}

export function AssignmentsSidebar() {
  const scheduledView = useUserPreferencesStore((s) => s.scheduledView);
  const setScheduledView = useUserPreferencesStore((s) => s.setScheduledView);
  const { assignments } = useAssignments();
  const firingToday = useMemo(() => assignmentsFiringToday(assignments), [assignments]);

  return (
    <div className="flex flex-col flex-1 min-h-0">
      {/* Calendar / List toggle — stacked rows, each taking the sidebar's full
          width rather than splitting a side-by-side segmented pill. */}
      <div className="mb-[8px] flex flex-col gap-[2px]">
        <ViewToggleButton
          active={scheduledView === "calendar"}
          onClick={() => setScheduledView("calendar")}
          icon={CalendarDays}
          label="Calendar"
        />
        <ViewToggleButton
          active={scheduledView === "list"}
          onClick={() => setScheduledView("list")}
          icon={ListChecks}
          label="List"
        />
      </div>

      {/* "New assignment" entry point — opens the trigger-aware create modal
          with no agent preselected (the modal offers an owner picker). */}
      <button
        type="button"
        onClick={() => useAssignmentEditorModalStore.getState().openCreate()}
        title="Create a new assignment"
        className="mx-[4px] flex items-center gap-[6px] px-[8px] py-[6px] rounded-[10px] text-[15px] font-medium text-[var(--text-secondary)] hover:text-[var(--sidebar-text-primary,var(--text-primary))] hover:bg-[var(--bg-hover)] transition-colors cursor-pointer"
      >
        <Plus size={15} />
        New assignment
      </button>

      <div className="mt-[8px] pt-[8px] border-t border-[var(--border-secondary)] flex-1 min-h-0 flex flex-col overflow-y-auto">
        <div className="mx-[4px] px-[8px] mb-[2px] text-[11px] font-semibold uppercase tracking-wide text-[var(--text-tertiary)]">
          Firing today
        </div>
        {firingToday.length === 0 ? (
          <div className="mx-[4px] px-[8px] py-[4px] text-[12px] text-[var(--text-tertiary)]">
            Nothing scheduled today
          </div>
        ) : (
          firingToday.map((a) => <TodayAssignmentRow key={a.id} assignment={a} />)
        )}
      </div>
    </div>
  );
}
