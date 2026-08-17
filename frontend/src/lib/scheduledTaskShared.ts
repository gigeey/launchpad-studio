import type { ScheduledTask } from "./api";

// ---------------------------------------------------------------------------
// Data shapes + pure helpers shared by every surface that still renders a
// `ScheduledTask` shape — the calendar grid (ScheduledCalendar), its owner
// chip (ScheduledTaskOwnerChip), and the Assignments adapters that project an
// `Assignment` onto this shape (assignmentToScheduledTaskWithOwner) so that
// calendar/occurrence-expansion machinery is reused verbatim rather than
// re-implemented. The stateful aggregate-fetch hook that used to live
// alongside these (the legacy Scheduled page's `useScheduledTasks`) is gone —
// Assignments owns data-fetching now — but the shapes themselves are still
// load-bearing, so they live on here independent of any hook.
// ---------------------------------------------------------------------------

/**
 * The resolved owner of a scheduled task — either an agent or a team. This is
 * the stable contract the calendar view, owner chip, and Assignments adapters
 * all build on. Deliberately excludes color: that's resolved at render time
 * via agentAvatarColor/teamTintColor in ./agentColors so this stays a pure
 * data shape.
 */
export interface ScheduledTaskOwner {
  id: string;
  name: string;
  emoji?: string;
  isTeam: boolean;
}

/** A ScheduledTask with its owner already resolved. */
export interface ScheduledTaskWithOwner extends ScheduledTask {
  owner: ScheduledTaskOwner;
}

/** The label to show for a scheduled task wherever a single identifying
 *  string is needed (calendar tiles, tooltips): the user-supplied `name` when
 *  set, falling back to the raw `prompt` for unnamed tasks (including every
 *  task created before this field existed). */
export function scheduledTaskDisplayLabel(task: Pick<ScheduledTask, "name" | "prompt">): string {
  return task.name?.trim() || task.prompt;
}
