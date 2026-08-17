import cronstrue from "cronstrue";
import type { Assignment } from "../../lib/api";
import type { ScheduledTaskWithOwner } from "../../lib/scheduledTaskShared";
import type { AssignmentWithOwner } from "../../hooks/useAssignments";

// ---------------------------------------------------------------------------
// Adapters bridging the `Assignment` data model onto the schedule surfaces that
// were originally built for `ScheduledTask`.
//
// Two responsibilities, kept pure so both the calendar and the list can share
// them:
//   1. Trigger classification + human-readable labels — a schedule (cron)
//      assignment belongs on the calendar; every other trigger surfaces in the
//      list with a plain-language description of *when* it fires.
//   2. Projecting a cron assignment onto the `ScheduledTaskWithOwner` shape the
//      existing calendar (ScheduledCalendar) + occurrence-expansion logic
//      already consume, so that machinery is reused verbatim rather than
//      re-implemented.
// ---------------------------------------------------------------------------

/** Only schedule-triggered (cron) assignments have a future fire series, so
 *  only these ever appear on the calendar. */
export function isCronAssignment(a: Assignment): boolean {
  return a.trigger.type === "Cron";
}

/** Human description of a cron expression, or null if cronstrue can't parse it. */
function cronDescription(expr: string): string | null {
  try {
    return cronstrue.toString(expr, { use24HourTimeFormat: false });
  } catch {
    return null;
  }
}

/**
 * Plain-language "when this fires" label for an assignment's trigger — the
 * spine of the unified surface. Schedule triggers describe their
 * cadence; event triggers describe their source.
 */
export function assignmentTriggerLabel(a: Assignment): string {
  switch (a.trigger.type) {
    case "Cron": {
      if (a.trigger.is_recurring) {
        return cronDescription(a.trigger.cron_expr) ?? a.trigger.cron_expr;
      }
      return "One time";
    }
    case "Webhook":
      return "When POSTed to its webhook URL";
    case "ConnectorEvent":
      return `Polling ${a.trigger.server_name} every ${a.trigger.poll_interval_secs}s`;
    case "AgentWatch":
      return `Watching every ${a.trigger.poll_interval_secs}s`;
    default:
      // Exhaustiveness guard: a future trigger variant shows a generic label
      // instead of silently rendering nothing.
      return "Custom trigger";
  }
}

/**
 * Project a resolved assignment onto the `ScheduledTaskWithOwner` shape the
 * calendar grid + `scheduleOccurrences` helpers read. A cron trigger carries
 * its expression and recurrence through; any non-cron trigger maps to a
 * non-recurring, unscheduled task (`cron: null`, `is_recurring: false`) so it
 * simply never materializes an occurrence on the grid.
 */
export function assignmentToScheduledTaskWithOwner(
  a: AssignmentWithOwner,
): ScheduledTaskWithOwner {
  const cron = a.trigger.type === "Cron" ? a.trigger.cron_expr : null;
  const isRecurring = a.trigger.type === "Cron" ? a.trigger.is_recurring : false;
  return {
    id: a.id,
    agent_id: a.agent_id,
    name: a.name,
    is_team: false,
    cron,
    prompt: a.instruction,
    working_directory: a.working_directory ?? null,
    is_recurring: isRecurring,
    created_at: a.created_ts,
    last_run_at: a.last_run_at ?? null,
    next_fire_at: a.next_fire_at ?? null,
    enabled: a.enabled,
    expires_at: a.expires_at ?? null,
    thread_policy: a.thread_policy,
    dedicated_thread_id: a.dedicated_thread_id ?? null,
    owner: a.owner,
  };
}
