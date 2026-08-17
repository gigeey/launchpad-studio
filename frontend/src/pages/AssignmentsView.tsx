import { useCallback, useMemo } from "react";
import { useNavigate } from "react-router-dom";
import { CalendarClock } from "lucide-react";
import { useAssignments, type AssignmentOwner } from "../hooks/useAssignments";
import { useUserPreferencesStore } from "../stores/userPreferencesStore";
import { useChatStore } from "../stores/chatStore";
import { switchToThread } from "../lib/threadNavigation";
import { ScheduledCalendar } from "../components/scheduled/ScheduledCalendar";
import { AssignmentList } from "../components/assignments/AssignmentList";
import { assignmentToScheduledTaskWithOwner } from "../components/assignments/assignmentAdapters";
import { useAssignmentEditorModalStore } from "../stores/assignmentEditorModalStore";
import { ContentGate } from "../components/ContentGate";
import { SidebarListSkeleton } from "../components/shared/Skeletons";
import { useReadyLatch } from "../hooks/useReadyLatch";

// ---------------------------------------------------------------------------
// Aggregate "Assignments" page — one surface for every assignment across every
// agent. Successor to the Scheduled page: the schedule (cron) trigger keeps its
// rich calendar, and the list view additionally carries webhook / future
// connector-event assignments that have no fire series to plot.
//
// Calendar-vs-list split:
//   • Calendar view — cron assignments only, projected onto the existing
//     ScheduledCalendar via `assignmentToScheduledTaskWithOwner` so the
//     occurrence-expansion + timezone machinery is reused verbatim. Non-cron
//     triggers are omitted here (nothing to place on a date).
//   • List view — EVERY assignment, each footered with a human-readable trigger
//     label (cron cadence, or "When POSTed to …" for webhooks).
//
// The Calendar/List toggle itself lives in AssignmentsSidebar and is backed by
// the shared `scheduledView` preference. Creating/editing assignments is driven
// by the AssignmentEditorModal, which is mounted once at the app shell root
// (so the same button works from Chat/Home too) — this view just refetches
// whenever the modal's `savedAt` counter changes, via useAssignments().
// ---------------------------------------------------------------------------

export function AssignmentsView() {
  const { assignments, loading, error, refetch } = useAssignments();
  const scheduledView = useUserPreferencesStore((s) => s.scheduledView);
  const navigate = useNavigate();
  const triggerAssignment = useChatStore((s) => s.triggerAssignment);

  // Selecting an assignment (from calendar or list) opens the trigger-aware
  // editor scoped to its owning agent. `owner.id` is that agent's id.
  const handleSelect = useCallback((owner: AssignmentOwner, assignmentId: string) => {
    useAssignmentEditorModalStore.getState().openEdit(owner.id, assignmentId);
  }, []);

  // Fire an assignment straight from its list tile — the same manual,
  // foreground-intent trigger the editor modal's "Run now" performs, surfaced
  // where the assignments actually live so it's reachable without opening the
  // editor first. Identical sequence to the modal handler: trigger, then (the
  // run already carries a resolved `thread_id`, no SSE wait) switch to that
  // thread and navigate into the owning agent's chat so the run streams live.
  const handleRunNow = useCallback(
    async (owner: AssignmentOwner, assignmentId: string) => {
      const agentId = owner.id;
      const run = await triggerAssignment(assignmentId);
      if (run.thread_id) {
        await switchToThread(agentId, run.thread_id);
      }
      navigate(`/chat/${agentId}`);
    },
    [triggerAssignment, navigate],
  );

  // Only schedule-triggered assignments have occurrences to plot on the grid.
  const calendarTasks = useMemo(
    () =>
      assignments
        .filter((a) => a.trigger.type === "Cron")
        .map(assignmentToScheduledTaskWithOwner),
    [assignments],
  );

  const isEmpty = !loading && !error && assignments.length === 0;
  const ready = useReadyLatch(assignments.length > 0, loading);

  return (
    <div className="flex flex-col flex-1 min-h-0 bg-[var(--bg-secondary)]">
      <ContentGate ready={ready} skeleton={<SidebarListSkeleton rows={5} />} className="flex-1 min-h-0">
        {error ? (
          <div className="flex flex-col items-center justify-center h-full gap-2 text-[var(--text-secondary)]">
            <CalendarClock size={44} className="opacity-40" />
            <p className="text-[14px] font-medium">{error}</p>
            <button
              type="button"
              onClick={() => void refetch()}
              className="mt-1 text-[13px] font-medium text-[var(--accent)] hover:underline cursor-pointer"
            >
              Try again
            </button>
          </div>
        ) : isEmpty ? (
          <div className="flex flex-col items-center justify-center h-full gap-3 text-[var(--text-secondary)] px-6 text-center">
            <div className="w-20 h-20 rounded-3xl bg-[var(--text-secondary)] flex items-center justify-center">
              <CalendarClock size={42} className="text-[var(--bg-tertiary)]" strokeWidth={1.75} />
            </div>
            <p className="text-[15px] font-medium text-[var(--text-primary)]">No assignments yet</p>
            <p className="text-[13px] max-w-[320px] leading-[18px]">
              Assignments run an agent automatically on a trigger — a schedule, a webhook, and
              soon a connector event.
            </p>
          </div>
        ) : scheduledView === "calendar" ? (
          <ScheduledCalendar
            tasks={calendarTasks}
            onSelectTask={handleSelect}
            onNewForDate={(dateKey) =>
              useAssignmentEditorModalStore.getState().openCreate(undefined, dateKey)
            }
          />
        ) : (
          <AssignmentList assignments={assignments} onSelect={handleSelect} onRunNow={handleRunNow} />
        )}
      </ContentGate>
    </div>
  );
}
