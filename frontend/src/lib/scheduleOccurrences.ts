import { CronExpressionParser } from "cron-parser";
import type { ScheduledTask } from "./api";

// ---------------------------------------------------------------------------
// Occurrence expansion for the aggregate Scheduled page.
//
// The backend stores each task's *next* fire time (next_fire_at) but not the
// full future series. The calendar view needs every fire instant that lands in
// the visible month grid, so we recompute the series client-side from the cron
// expression. Cron is the standard 5-field form (minute hour day-of-month month
// day-of-week) — identical to what CronPicker emits and the backend's croner
// crate consumes, so no dialect translation is needed here.
// ---------------------------------------------------------------------------

/**
 * Hard cap on how many occurrences a single expansion will emit — a backstop
 * against a malformed or pathological expression (e.g. "every minute" over a
 * year-long range) spinning indefinitely or allocating an enormous array.
 *
 * Sized against the aggregate calendar's actual query shape rather than an
 * arbitrary round number: ScheduledCalendar only ever calls expandOccurrences
 * with the visible month grid (at most 42 cells) plus a day of slack on each
 * side (~45 days total), and only for tasks that fall *below*
 * FREQUENT_TASK_THRESHOLD_PER_DAY — anything at or above that threshold is
 * excluded from this per-instant expansion entirely and shown instead via
 * hasOccurrenceOnDay plus the calendar's frequent-task legend/corner markers.
 * A task just under the threshold (~8/day) over 45 days needs at most ~360
 * occurrences, so 1000 leaves comfortable headroom for calendar edge cases
 * (DST, short/long months) while still bounding a truly pathological
 * expression well short of "materialize the whole range".
 */
const MAX_OCCURRENCES = 1000;

/** Occurrences whose client/server disagreement exceeds this are worth a warn. */
const DRIFT_TOLERANCE_MS = 60_000;

/** How far ahead firstOccurrenceDriftWarning looks for the next fire instant. */
const DRIFT_LOOKAHEAD_MS = 366 * 24 * 60 * 60 * 1000;

/** Task ids we've already warned about, so the diagnostic fires at most once. */
const driftWarned = new Set<string>();

function pad2(n: number): string {
  return n.toString().padStart(2, "0");
}

/**
 * Civil-date key (YYYY-MM-DD) for the wall-clock day an instant falls on in
 * `tz`. Exported so both the calendar grid and the frequent-task helpers
 * below (hasOccurrenceOnDay, isFrequentTask) bucket occurrences by day the
 * same way.
 */
export function dayKeyInTz(date: Date, tz: string): string {
  try {
    // en-CA renders as YYYY-MM-DD, which sorts and compares cleanly.
    return new Intl.DateTimeFormat("en-CA", {
      timeZone: tz,
      year: "numeric",
      month: "2-digit",
      day: "2-digit",
    }).format(date);
  } catch {
    // Fall back to the browser-local civil day if the tz string is invalid.
    return `${date.getFullYear()}-${pad2(date.getMonth() + 1)}-${pad2(date.getDate())}`;
  }
}

/**
 * Expand a scheduled task into the concrete fire instants that fall within
 * `[rangeStart, rangeEnd]` (both inclusive).
 *
 * - One-time task: returns its single `next_fire_at` if that instant is within
 *   the range, otherwise an empty array.
 * - Recurring task: parses `cron` in the given IANA `timezone` and walks
 *   forward from `rangeStart`, collecting every fire instant up to `rangeEnd`.
 *   Stops early at `expires_at` when set, and never emits more than
 *   MAX_OCCURRENCES entries.
 *
 * Any parse/iteration failure is swallowed and yields an empty array — the
 * calendar treats an unparseable schedule as "nothing to show" rather than
 * crashing, mirroring the defensive posture of cronDescription() in the
 * schedule modal.
 */
export function expandOccurrences(
  task: ScheduledTask,
  rangeStart: Date,
  rangeEnd: Date,
  timezone: string,
): Date[] {
  const startMs = rangeStart.getTime();
  const endMs = rangeEnd.getTime();
  if (Number.isNaN(startMs) || Number.isNaN(endMs) || startMs > endMs) return [];

  // One-time task: a single stored instant, included only if it's in range.
  if (!task.is_recurring) {
    if (!task.next_fire_at) return [];
    const fire = new Date(task.next_fire_at);
    const fireMs = fire.getTime();
    if (Number.isNaN(fireMs)) return [];
    return fireMs >= startMs && fireMs <= endMs ? [fire] : [];
  }

  // Recurring task with no cron expression has no series to expand.
  if (!task.cron) return [];

  const expiresMs = task.expires_at ? new Date(task.expires_at).getTime() : null;
  const effectiveEndMs =
    expiresMs !== null && !Number.isNaN(expiresMs) ? Math.min(endMs, expiresMs) : endMs;

  try {
    // currentDate is set one millisecond before rangeStart so a fire instant
    // landing exactly on rangeStart is still returned (next() is strictly
    // greater than currentDate). tz evaluates the cron fields in the user's
    // timezone; the resulting CronDate is an absolute instant either way.
    const parsed = CronExpressionParser.parse(task.cron, {
      currentDate: new Date(startMs - 1),
      tz: timezone,
    });

    const occurrences: Date[] = [];
    for (let i = 0; i < MAX_OCCURRENCES; i++) {
      if (!parsed.hasNext()) break;
      const next = parsed.next().toDate();
      if (next.getTime() > effectiveEndMs) break;
      occurrences.push(next);
    }
    return occurrences;
  } catch {
    return [];
  }
}

/**
 * Civil Y/M/D/H/M/S components of `instant` as evaluated in `tz`. Internal
 * helper for zonedWallTimeToUtc's round-trip below.
 */
function civilPartsInTz(
  instant: Date,
  tz: string,
): { y: number; mo: number; d: number; h: number; mi: number; s: number } {
  try {
    const parts = new Intl.DateTimeFormat("en-US", {
      timeZone: tz,
      year: "numeric",
      month: "2-digit",
      day: "2-digit",
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit",
      hour12: false,
    }).formatToParts(instant);
    const get = (type: string) => parseInt(parts.find((p) => p.type === type)?.value ?? "0", 10);
    // hour12:false can render midnight as "24" in some engines/locales — normalize back to 0.
    return { y: get("year"), mo: get("month"), d: get("day"), h: get("hour") % 24, mi: get("minute"), s: get("second") };
  } catch {
    return {
      y: instant.getUTCFullYear(),
      mo: instant.getUTCMonth() + 1,
      d: instant.getUTCDate(),
      h: instant.getUTCHours(),
      mi: instant.getUTCMinutes(),
      s: instant.getUTCSeconds(),
    };
  }
}

/**
 * Absolute UTC instant for the civil wall-clock time `y-mo-d h:mi:s` as it
 * would read in `tz`. Standard iterative round-trip — treat the wall time as
 * UTC, see what that instant actually reads as in `tz`, and correct by the
 * difference. Converges within a couple of passes even across a DST
 * transition; only used by hasOccurrenceOnDay below, where a couple of extra
 * Intl calls per (task, day) pair is a non-issue.
 */
function zonedWallTimeToUtc(y: number, mo: number, d: number, h: number, mi: number, s: number, tz: string): Date {
  const targetMs = Date.UTC(y, mo - 1, d, h, mi, s);
  let guessMs = targetMs;
  for (let i = 0; i < 3; i++) {
    const p = civilPartsInTz(new Date(guessMs), tz);
    const guessedWallMs = Date.UTC(p.y, p.mo - 1, p.d, p.h, p.mi, p.s);
    const diff = targetMs - guessedWallMs;
    if (diff === 0) break;
    guessMs += diff;
  }
  return new Date(guessMs);
}

/**
 * True if `task` fires at least once on the civil day `dayKey` (YYYY-MM-DD,
 * as produced by dayKeyInTz) in `timezone`. This is what the calendar's
 * frequent-task corner markers use to test day membership one cell at a
 * time, instead of expanding a dense cron's full occurrence list just to
 * answer a boolean.
 *
 * A cron field always carries at least one value, so the field's own
 * smallest hour/minute/second combination (cron-parser keeps `values` sorted
 * ascending) is the earliest instant `dayKey` could possibly fire at — *if*
 * the day-of-month/day-of-week/month fields match that day at all. Probing
 * that single instant with the library's own `includesDate()` (which already
 * implements the standard cron day-of-month/day-of-week OR/AND quirk, `L`/
 * `#` modifiers, and DST-aware instant matching) answers the question in
 * O(1), rather than walking every candidate instant between "a day before
 * dayKey" and dayKey itself — which is O(fires-per-day) and, for a dense cron
 * like "every minute", was measured at several *seconds* of blocked render
 * time across a 42-cell month grid before this rewrite.
 */
export function hasOccurrenceOnDay(task: ScheduledTask, dayKey: string, timezone: string): boolean {
  const [y, m, d] = dayKey.split("-").map((s) => parseInt(s, 10));
  if ([y, m, d].some((n) => Number.isNaN(n))) return false;

  if (!task.is_recurring) {
    if (!task.next_fire_at) return false;
    const fireMs = new Date(task.next_fire_at).getTime();
    return !Number.isNaN(fireMs) && dayKeyInTz(new Date(fireMs), timezone) === dayKey;
  }
  if (!task.cron) return false;

  try {
    const parsed = CronExpressionParser.parse(task.cron, { tz: timezone });
    const { hour, minute, second } = parsed.fields;
    const probe = zonedWallTimeToUtc(y, m, d, hour.values[0], minute.values[0], second.values[0] ?? 0, timezone);

    const expiresMs = task.expires_at ? new Date(task.expires_at).getTime() : null;
    if (expiresMs !== null && !Number.isNaN(expiresMs) && probe.getTime() > expiresMs) return false;

    return parsed.includesDate(probe);
  } catch {
    return false;
  }
}

/** Fires/day at or above this threshold are dense enough to threaten the
 *  per-day chip cap (useResponsiveChipCap in ScheduledCalendar.tsx tops out
 *  around 10/day, and a noisy task sorts first in every cell it appears in
 *  since occurrences are ascending by time) — these are excluded from
 *  per-instant calendar expansion and shown instead via a frequent-task
 *  legend row plus day-cell corner markers. Picked well above "twice a day"
 *  (e.g. a 12h cron) so a task firing a couple of times daily — where the
 *  specific time-of-day is genuinely useful information — still renders as
 *  normal chips. */
export const FREQUENT_TASK_THRESHOLD_PER_DAY = 8;

/** How far ahead isFrequentTask samples when classifying a task's density. */
const CLASSIFICATION_HORIZON_DAYS = 7;

/** Safety bound on the classification walk — stops long before this once any
 *  single sampled day has already crossed the threshold; only a very sparse
 *  cron (never classified as frequent) would run the walk out fully. */
const CLASSIFICATION_SCAN_LIMIT = 500;

/**
 * True if `task`'s cron fires at least FREQUENT_TASK_THRESHOLD_PER_DAY times
 * on any single civil day within the next CLASSIFICATION_HORIZON_DAYS days
 * (in `timezone`), counting from `now`. Sampling a week rather than a single
 * day means a cron that's only dense on, say, weekdays still classifies
 * correctly instead of depending on which one day happens to get checked.
 * One-time tasks and recurring tasks without a cron are never frequent.
 */
export function isFrequentTask(task: ScheduledTask, timezone: string, now: Date = new Date()): boolean {
  if (!task.is_recurring || !task.cron) return false;

  const horizonMs = now.getTime() + CLASSIFICATION_HORIZON_DAYS * 24 * 60 * 60 * 1000;
  const countsByDay = new Map<string, number>();

  try {
    const parsed = CronExpressionParser.parse(task.cron, {
      currentDate: new Date(now.getTime() - 1),
      tz: timezone,
    });
    for (let i = 0; i < CLASSIFICATION_SCAN_LIMIT; i++) {
      if (!parsed.hasNext()) return false;
      const next = parsed.next().toDate();
      if (next.getTime() > horizonMs) return false;
      const key = dayKeyInTz(next, timezone);
      const count = (countsByDay.get(key) ?? 0) + 1;
      if (count >= FREQUENT_TASK_THRESHOLD_PER_DAY) return true;
      countsByDay.set(key, count);
    }
    return false;
  } catch {
    return false;
  }
}

/**
 * Dev-console diagnostic: for a recurring, enabled task that already carries a
 * server-computed `next_fire_at`, recompute the next fire instant client-side
 * and warn (once per task id) if the two disagree by more than a minute.
 *
 * This surfaces cases where cron-parser and the backend's croner crate resolve
 * the same expression differently on some dialect edge case, before that
 * mismatch quietly corrupts the calendar. It is purely diagnostic: it never
 * throws and is never wired into a UI blocking path.
 */
export function firstOccurrenceDriftWarning(task: ScheduledTask, timezone: string): void {
  try {
    if (!task.is_recurring || !task.enabled || !task.cron || !task.next_fire_at) return;
    if (driftWarned.has(task.id)) return;

    const now = new Date();
    const horizon = new Date(now.getTime() + DRIFT_LOOKAHEAD_MS);
    const upcoming = expandOccurrences(task, now, horizon, timezone);
    if (upcoming.length === 0) return;

    const storedMs = new Date(task.next_fire_at).getTime();
    if (Number.isNaN(storedMs)) return;

    const clientMs = upcoming[0].getTime();
    if (Math.abs(clientMs - storedMs) > DRIFT_TOLERANCE_MS) {
      driftWarned.add(task.id);
      console.warn(
        `[scheduleOccurrences] next-fire drift for task ${task.id} (cron "${task.cron}"): ` +
          `client computed ${upcoming[0].toISOString()} but backend stored ${new Date(
            storedMs,
          ).toISOString()}.`,
      );
    }
  } catch {
    // Diagnostic only — never let a drift check break rendering.
  }
}
