import type { TaskStatus } from "../types/api";

/**
 * Per-task "running since" tracking for the live elapsed-time indicator shown
 * on in-progress tasks.
 *
 * The wire `Task` carries no start timestamp, so we capture the moment a task
 * first enters `in_progress` on the client — observed through the tasklist SSE
 * stream and the initial hydrate — and persist it to localStorage. Persisting
 * means a page reload mid-run keeps the timer accurate instead of resetting to
 * zero.
 *
 * A task that never passed through an observed `in_progress` transition on this
 * client (e.g. it began while the tab was closed) has no recorded origin; the
 * caller falls back to stamping the first observation, so the timer still
 * advances — just from a slightly later origin. Any non-running state drops the
 * entry, which both bounds storage and resets the origin cleanly if the task is
 * later re-dispatched.
 */

const STORAGE_KEY = "ao.taskRunTimers.v1";

/** Upper bound on retained origins. Terminal transitions normally clear their
 *  own entry, so this only trips when many runs ended without the client ever
 *  observing their terminal event (tab closed mid-run). When tripped we keep
 *  the most-recent half and drop the rest. */
const MAX_ENTRIES = 500;
const TRIM_TO = 250;

type StartMap = Record<string, number>;

function compositeKey(tasklistId: string, taskId: string): string {
  return `${tasklistId}::${taskId}`;
}

function load(): StartMap {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return {};
    const parsed: unknown = JSON.parse(raw);
    if (!parsed || typeof parsed !== "object") return {};
    return parsed as StartMap;
  } catch {
    return {};
  }
}

function persist(map: StartMap): void {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(map));
  } catch {
    /* storage unavailable / over quota — degrade to in-memory only */
  }
}

let cache: StartMap = load();

/**
 * Record (or clear) a task's run-start origin from its latest status.
 *
 * Idempotent for the running case: an already-recorded in-progress task keeps
 * its original origin, so repeated SSE updates never reset the clock.
 */
export function noteTaskStatus(
  tasklistId: string,
  taskId: string,
  status: TaskStatus,
): void {
  const key = compositeKey(tasklistId, taskId);

  if (status === "in_progress") {
    if (cache[key] != null) return;
    cache[key] = Date.now();
    // Safety valve: if terminal-clear events were missed, keep the map bounded
    // by retaining only the most-recently-started origins.
    const keys = Object.keys(cache);
    if (keys.length > MAX_ENTRIES) {
      const trimmed: StartMap = {};
      for (const k of keys.sort((a, b) => cache[b] - cache[a]).slice(0, TRIM_TO)) {
        trimmed[k] = cache[k];
      }
      cache = trimmed;
    }
    persist(cache);
    return;
  }

  // Any non-running state (pending, blocked, completed, failed, skipped) is not
  // actively timing — drop the origin.
  if (cache[key] != null) {
    delete cache[key];
    persist(cache);
  }
}

/** Epoch-ms origin for a running task, or null when none is recorded. */
export function getTaskRunStart(
  tasklistId: string,
  taskId: string,
): number | null {
  return cache[compositeKey(tasklistId, taskId)] ?? null;
}

/**
 * Format an elapsed millisecond span as a compact label (`45s`, `2m 05s`,
 * `1h 03m`). Matches the convention used by the streaming tool indicators so
 * running times read the same across the product.
 */
export function formatRunElapsed(ms: number): string {
  const totalSeconds = Math.max(0, Math.floor(ms / 1000));
  if (totalSeconds < 60) return `${totalSeconds}s`;

  const totalMinutes = Math.floor(totalSeconds / 60);
  if (totalMinutes < 60) {
    const secs = totalSeconds % 60;
    return `${totalMinutes}m ${String(secs).padStart(2, "0")}s`;
  }

  const hours = Math.floor(totalMinutes / 60);
  const mins = totalMinutes % 60;
  return `${hours}h ${String(mins).padStart(2, "0")}m`;
}

/**
 * Total wall-clock span a tasklist has been (or was) running, in milliseconds.
 *
 * Unlike the per-task origins above, the tasklist carries real backend
 * timestamps, so this needs no client-side capture and stays accurate across
 * reloads. The anchor is `created_at` (when the list came into being and, for
 * the agent surface, began running). While the list is live we measure to
 * `now`; once it stops we freeze at `last_active_at` — the timestamp the
 * backend stamps on the final transition — falling back to `created_at` when
 * the list never recorded activity.
 *
 * Returns null when `created_at` can't be parsed so the caller can omit the
 * indicator rather than render a bogus span.
 */
export function computeTasklistElapsedMs(
  createdAt: string,
  lastActiveAt: string | null | undefined,
  isRunning: boolean,
  now: number,
): number | null {
  const start = Date.parse(createdAt);
  if (Number.isNaN(start)) return null;

  let end: number;
  if (isRunning) {
    end = now;
  } else {
    const last = lastActiveAt ? Date.parse(lastActiveAt) : NaN;
    end = Number.isNaN(last) ? start : last;
  }

  return Math.max(0, end - start);
}
