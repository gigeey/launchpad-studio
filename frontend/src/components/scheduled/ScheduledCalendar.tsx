import { useEffect, useMemo, useRef, useState } from "react";
import { ChevronLeft, ChevronRight, Plus, X } from "lucide-react";
import { AnimatePresence, motion } from "framer-motion";
import type { ScheduledTaskOwner, ScheduledTaskWithOwner } from "../../lib/scheduledTaskShared";
import { scheduledTaskDisplayLabel } from "../../lib/scheduledTaskShared";
import {
  dayKeyInTz,
  expandOccurrences,
  hasOccurrenceOnDay,
  isFrequentTask,
} from "../../lib/scheduleOccurrences";
import { frequentTaskColor } from "../../lib/agentColors";
import { useIsDark, useUserPreferencesStore } from "../../stores/userPreferencesStore";
import { ownerColor, ownerColorVibrant } from "./ScheduledTaskOwnerChip";

// ---------------------------------------------------------------------------
// Month-grid calendar for the aggregate Scheduled page.
//
// Pure presentational consumer: the parent owns the task list and passes it in.
// For every task we recompute its fire instants across the visible grid (via
// expandOccurrences, evaluated in the user's timezone) and bucket them into day
// cells. There is deliberately no calendar-grid dependency here — the grid is
// hand-rolled with plain calendar arithmetic, in the same spirit as the
// hand-built CronPicker / DateTimePicker in this area.
// ---------------------------------------------------------------------------

const WEEKDAY_LABELS = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTH_LABELS = [
  "January", "February", "March", "April", "May", "June",
  "July", "August", "September", "October", "November", "December",
];

/**
 * Chip cap before a day cell collapses to "+N more" — sized dynamically from
 * the actual rendered cell height (see `useResponsiveChipCap` below) rather
 * than a fixed constant, so a tall window shows more occurrences per day and a
 * short one doesn't render chips bunched on top of each other. This is only
 * the initial value used before the first measurement lands.
 */
const DEFAULT_MAX_CHIPS_PER_DAY = 3;
const MIN_CHIPS_PER_DAY = 1;
const MAX_CHIPS_PER_DAY_CEILING = 10;

/** Corner markers shown inline per day cell before the rest fold into a
 *  "+N" indicator (title-tooltip listing the overflow) — keeps the
 *  date-number row's right-hand slot from growing unbounded on a day where
 *  many frequent tasks happen to overlap. */
const MAX_CORNER_MARKERS = 4;

/** Swatches shown inline in the legend row before the rest fold into a "+N"
 *  indicator — same reasoning as MAX_CORNER_MARKERS, applied to the header
 *  strip instead of a single day cell. */
const MAX_LEGEND_SWATCHES = 12;

// Pixel estimates of the day-cell chrome, used to back out how many chip rows
// fit in the remaining space. These mirror the Tailwind classes on the cell
// (`p-1.5`, the date-number row, the `gap-1` between it and the chip stack)
// and on each chip button (`py-0.5` + line height + the `gap-[3px]` stack
// spacing). The date-number row is pinned to a fixed height (`h-[18px]`) on
// the element itself rather than left to font metrics, so this constant stays
// exact regardless of the date number's font size. A full extra chip row is
// reserved so there's always room for the "+N more" button on days that
// overflow, instead of it fighting the last chip for space.
const CELL_PADDING_Y = 12;
const DATE_ROW_HEIGHT = 18;
const HEADER_GAP = 4;
const CHIP_ROW_HEIGHT = 21;

/**
 * Measures the rendered month-grid container and derives how many occurrence
 * chips a day cell can show before it needs to collapse to "+N more" — so the
 * cap scales with the window/grid size (a taller grid means taller cells)
 * instead of being a fixed number that looks bunched up on a compact grid.
 */
function useResponsiveChipCap(rows: number): [React.RefObject<HTMLDivElement | null>, number] {
  const gridRef = useRef<HTMLDivElement | null>(null);
  const [cap, setCap] = useState(DEFAULT_MAX_CHIPS_PER_DAY);

  useEffect(() => {
    const el = gridRef.current;
    if (!el) return;

    const recalc = () => {
      const cellHeight = el.clientHeight / Math.max(1, rows);
      const available = cellHeight - CELL_PADDING_Y - DATE_ROW_HEIGHT - HEADER_GAP;
      // Reserve one row's worth of space for a potential "+N more" button.
      const raw = Math.floor(available / CHIP_ROW_HEIGHT) - 1;
      setCap(Math.max(MIN_CHIPS_PER_DAY, Math.min(MAX_CHIPS_PER_DAY_CEILING, raw)));
    };

    recalc();
    const ro = new ResizeObserver(recalc);
    ro.observe(el);
    return () => ro.disconnect();
  }, [rows]);

  return [gridRef, cap];
}

function pad2(n: number): string {
  return n.toString().padStart(2, "0");
}

/** Short local time label (e.g. "9:00 AM") for an instant, rendered in `tz`. */
function timeLabelInTz(date: Date, tz: string): string {
  try {
    return new Intl.DateTimeFormat(undefined, {
      timeZone: tz,
      hour: "numeric",
      minute: "2-digit",
    }).format(date);
  } catch {
    return date.toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" });
  }
}

interface DayCell {
  key: string; // civil YYYY-MM-DD
  day: number; // day-of-month number to display
  inMonth: boolean; // belongs to the visible month (vs. leading/trailing padding)
  isToday: boolean;
}

/** One task firing at one instant, pre-resolved for a day cell. */
interface Occurrence {
  task: ScheduledTaskWithOwner;
  at: Date;
}

/**
 * Build the Sun–Sat padded grid of civil day cells for `visibleMonth`, marking
 * which cell (if any) is "today" in the user's timezone.
 */
function buildGrid(visibleMonth: Date, tz: string): DayCell[] {
  const year = visibleMonth.getFullYear();
  const month = visibleMonth.getMonth();
  const todayKey = dayKeyInTz(new Date(), tz);

  const firstWeekday = new Date(year, month, 1).getDay(); // 0 = Sun
  const daysInMonth = new Date(year, month + 1, 0).getDate();
  const totalCells = Math.ceil((firstWeekday + daysInMonth) / 7) * 7;

  const cells: DayCell[] = [];
  for (let i = 0; i < totalCells; i++) {
    // Local calendar arithmetic; the constructor normalizes month rollover.
    const cellDate = new Date(year, month, 1 - firstWeekday + i);
    const cy = cellDate.getFullYear();
    const cm = cellDate.getMonth();
    const cd = cellDate.getDate();
    const key = `${cy}-${pad2(cm + 1)}-${pad2(cd)}`;
    cells.push({
      key,
      day: cd,
      inMonth: cm === month,
      isToday: key === todayKey,
    });
  }
  return cells;
}

/** First-of-month for whatever month contains "now" in the given timezone. */
function currentMonthStart(tz: string): Date {
  const key = dayKeyInTz(new Date(), tz); // YYYY-MM-DD
  const [y, m] = key.split("-").map((s) => parseInt(s, 10));
  return new Date(y, m - 1, 1);
}

interface ScheduledCalendarProps {
  tasks: ScheduledTaskWithOwner[];
  onSelectTask: (owner: ScheduledTaskOwner, taskId: string) => void;
  /** Handler for a day cell's hover "+" (create on that date). Optional and
   *  backward-compatible: when omitted, the button opens the legacy
   *  ScheduledTask create modal exactly as before. The Assignments surface
   *  passes its own handler so the grid's create entry point isn't cross-wired
   *  to the superseded ScheduledTask flow. */
  onNewForDate?: (dateKey: string) => void;
}

export function ScheduledCalendar({ tasks, onSelectTask, onNewForDate }: ScheduledCalendarProps) {
  const timezone = useUserPreferencesStore((s) => s.timezone);
  const [visibleMonth, setVisibleMonth] = useState<Date>(() => currentMonthStart(timezone));
  const [openDayKey, setOpenDayKey] = useState<string | null>(null);

  const cells = useMemo(() => buildGrid(visibleMonth, timezone), [visibleMonth, timezone]);
  const rows = cells.length / 7;
  const [gridRef, maxChipsPerDay] = useResponsiveChipCap(rows);

  // Split off tasks dense enough to threaten the per-day chip cap — they're
  // excluded from the normal per-instant expansion below and shown instead
  // via a legend row + day-cell corner markers (see FrequentTaskLegend /
  // CornerMarker). A noisy task sorts first in every cell it appears in
  // (occurrences are ascending by time), so leaving it in occurrencesByDay
  // would bury legitimate daily/weekly events under "+N more" every day.
  const { normalTasks, frequentTasks } = useMemo(() => {
    const normal: ScheduledTaskWithOwner[] = [];
    const frequent: ScheduledTaskWithOwner[] = [];
    for (const task of tasks) {
      (isFrequentTask(task, timezone) ? frequent : normal).push(task);
    }
    return { normalTasks: normal, frequentTasks: frequent };
  }, [tasks, timezone]);

  // Bucket every normal task's occurrences into day cells, sorted by fire time.
  const occurrencesByDay = useMemo(() => {
    const cellKeys = new Set(cells.map((c) => c.key));
    const buckets = new Map<string, Occurrence[]>();
    if (cells.length === 0) return buckets;

    // Expand across the full visible span with a day of slack on each side so
    // timezone offsets can't push a boundary fire outside the query window;
    // the day-key bucketing below drops anything that isn't a visible cell.
    const first = cells[0].key.split("-").map((s) => parseInt(s, 10));
    const last = cells[cells.length - 1].key.split("-").map((s) => parseInt(s, 10));
    const gridStart = new Date(Date.UTC(first[0], first[1] - 1, first[2] - 1));
    const gridEnd = new Date(Date.UTC(last[0], last[1] - 1, last[2] + 2));

    for (const task of normalTasks) {
      const fires = expandOccurrences(task, gridStart, gridEnd, timezone);
      for (const at of fires) {
        const key = dayKeyInTz(at, timezone);
        if (!cellKeys.has(key)) continue;
        const list = buckets.get(key);
        if (list) list.push({ task, at });
        else buckets.set(key, [{ task, at }]);
      }
    }

    for (const list of buckets.values()) {
      list.sort((a, b) => a.at.getTime() - b.at.getTime());
    }
    return buckets;
  }, [cells, normalTasks, timezone]);

  // For each frequent task, which visible day cells it fires on — a cheap
  // existence check per (task, day) pair via hasOccurrenceOnDay rather than
  // reusing expandOccurrences (which would materialize every instant of a
  // dense cron just to answer a boolean).
  const cornersByDay = useMemo(() => {
    const map = new Map<string, ScheduledTaskWithOwner[]>();
    if (frequentTasks.length === 0) return map;
    for (const cell of cells) {
      const hits: ScheduledTaskWithOwner[] = [];
      for (const task of frequentTasks) {
        if (hasOccurrenceOnDay(task, cell.key, timezone)) hits.push(task);
      }
      if (hits.length > 0) map.set(cell.key, hits);
    }
    return map;
  }, [cells, frequentTasks, timezone]);

  // Local (non-persisted) dismissal of the frequent-task legend row.
  const [legendDismissed, setLegendDismissed] = useState(false);

  // Close the overflow popover when the visible month changes.
  useEffect(() => {
    setOpenDayKey(null);
  }, [visibleMonth]);

  const goPrev = () => setVisibleMonth((d) => new Date(d.getFullYear(), d.getMonth() - 1, 1));
  const goNext = () => setVisibleMonth((d) => new Date(d.getFullYear(), d.getMonth() + 1, 1));
  const goToday = () => setVisibleMonth(currentMonthStart(timezone));

  const navButtonClass =
    "flex items-center justify-center h-[32px] px-3 rounded-[8px] border border-[var(--border-primary)] bg-[var(--bg-secondary)] text-[13px] font-semibold text-[var(--text-primary)] hover:bg-[var(--bg-hover)] transition-colors cursor-pointer";

  return (
    <div className="flex flex-col h-full min-h-0 bg-[var(--bg-secondary)]">
      {/* Header: month label + frequent-task legend + navigation */}
      <div className="flex items-center justify-between px-4 py-3 flex-shrink-0 gap-3">
        <h2 className="text-[18px] font-bold flex-shrink-0" style={{ color: "var(--text-primary)" }}>
          {MONTH_LABELS[visibleMonth.getMonth()]} {visibleMonth.getFullYear()}
        </h2>
        <div className="flex items-center gap-3 min-w-0">
          <FrequentTaskLegend
            tasks={frequentTasks}
            dismissed={legendDismissed}
            onDismiss={() => setLegendDismissed(true)}
            onSelect={onSelectTask}
          />
          <div className="flex items-center gap-2 flex-shrink-0">
            <button type="button" onClick={goToday} className={navButtonClass}>
              Today
            </button>
            <button
              type="button"
              onClick={goPrev}
              aria-label="Previous month"
              className="flex items-center justify-center w-[32px] h-[32px] rounded-[8px] border border-[var(--border-primary)] bg-[var(--bg-secondary)] hover:bg-[var(--bg-hover)] transition-colors cursor-pointer"
              style={{ color: "var(--text-secondary)" }}
            >
              <ChevronLeft size={18} />
            </button>
            <button
              type="button"
              onClick={goNext}
              aria-label="Next month"
              className="flex items-center justify-center w-[32px] h-[32px] rounded-[8px] border border-[var(--border-primary)] bg-[var(--bg-secondary)] hover:bg-[var(--bg-hover)] transition-colors cursor-pointer"
              style={{ color: "var(--text-secondary)" }}
            >
              <ChevronRight size={18} />
            </button>
          </div>
        </div>
      </div>

      {/* Weekday header row */}
      <div
        className="grid grid-cols-7 flex-shrink-0 border-b"
        style={{ borderColor: "var(--border-secondary)" }}
      >
        {WEEKDAY_LABELS.map((label) => (
          <div
            key={label}
            className="px-2 py-2 text-[11px] font-semibold uppercase tracking-wide text-center"
            style={{ color: "var(--text-secondary)" }}
          >
            {label}
          </div>
        ))}
      </div>

      {/* Month grid. `gridAutoRows: minmax(0, 1fr)` (set inline, not via the
          Tailwind `auto-rows-fr` utility) is load-bearing: Tailwind's class
          compiles to a bare `1fr`, which still lets a row's automatic minimum
          size grow to fit its tallest cell's content — so a day with a lot of
          occurrences could force every row taller than the available space
          and overflow the page. `minmax(0, ...)` removes that implicit
          minimum so rows always tile the container exactly, no matter what. */}
      <div
        ref={gridRef}
        className="grid grid-cols-7 flex-1 min-h-0"
        style={{ gridAutoRows: "minmax(0, 1fr)" }}
      >
        {cells.map((cell) => {
          const occ = occurrencesByDay.get(cell.key) ?? [];
          const visible = occ.slice(0, maxChipsPerDay);
          const overflow = occ.slice(maxChipsPerDay);
          const corners = cornersByDay.get(cell.key) ?? [];
          const visibleCorners = corners.slice(0, MAX_CORNER_MARKERS);
          const overflowCorners = corners.slice(MAX_CORNER_MARKERS);

          return (
            <div
              key={cell.key}
              className="group relative flex flex-col gap-1 p-1.5 border-b border-r overflow-hidden"
              style={{
                borderColor: "var(--border-secondary)",
                background: "transparent",
                boxShadow: cell.isToday ? "inset 0 0 0 2px var(--accent)" : undefined,
              }}
            >
              {/* Date number — swaps to a "+" on hover, IN PLACE, so the "+"
                  reads as replacing the number rather than as a third thing
                  appearing next to the corner-marker dots. The number and
                  the "+" button are stacked via `absolute inset-0` inside a
                  shared `relative` box (sized to the button's own
                  w-[18px] h-[18px] footprint) instead of being two siblings
                  in the row's flex flow — that's what pins the "+" to the
                  number's own left-hand slot instead of letting it land
                  after the corner-marker dots on the right.
                  Hidden via `invisible` (visibility: hidden), NOT `hidden`
                  (display: none): this row is `justify-between` with the
                  number/+-swap box and the corner-markers group as its only
                  two flex children. `hidden` would remove the number from
                  the flex flow, leaving the marker group as the sole child —
                  which collapses `justify-between` to flex-start and snaps
                  the corner-marker dots to the left edge of the cell on
                  every hover. `invisible` keeps its box in the flex layout
                  so the marker group never has to reflow. */}
              <div className="flex-shrink-0 h-[18px] flex items-center justify-between">
                <div className="relative flex-shrink-0 w-[18px] h-[18px]">
                  <span
                    className="absolute inset-0 flex items-center justify-start text-[16px] font-semibold leading-none group-hover:invisible"
                    style={{
                      color: cell.isToday
                        ? "var(--accent)"
                        : cell.inMonth
                          ? "var(--text-primary)"
                          : "var(--text-secondary)",
                      opacity: cell.inMonth ? 1 : 0.6,
                    }}
                  >
                    {cell.day}
                  </span>
                  <button
                    type="button"
                    onClick={(e) => {
                      e.stopPropagation();
                      onNewForDate?.(cell.key);
                    }}
                    aria-label={`New scheduled item on ${MONTH_LABELS[parseInt(cell.key.split("-")[1], 10) - 1]} ${cell.day}`}
                    title="New scheduled item"
                    className="absolute inset-0 hidden group-hover:flex items-center justify-center w-[18px] h-[18px] rounded-[5px] bg-[var(--bg-hover)] text-[var(--accent)] hover:brightness-110 transition-colors cursor-pointer"
                  >
                    <Plus size={13} strokeWidth={2.5} />
                  </button>
                </div>
                {/* Right-hand slot: frequent-task corner markers only. The
                    "+" button used to live here too, which read as it
                    appearing next to the dots rather than replacing the
                    date number — it now lives in the number's own box
                    above. */}
                <div className="flex items-center gap-[2px] flex-shrink-0">
                  {visibleCorners.map((task) => (
                    <CornerMarker key={task.id} task={task} onSelect={onSelectTask} />
                  ))}
                  {overflowCorners.length > 0 && (
                    <span
                      className="text-[8px] font-bold leading-none flex-shrink-0"
                      style={{ color: "var(--text-secondary)" }}
                      title={overflowCorners
                        .map((t) => `${t.owner.name} · ${scheduledTaskDisplayLabel(t)}`)
                        .join(", ")}
                    >
                      +{overflowCorners.length}
                    </span>
                  )}
                </div>
              </div>

              {/* Occurrence chips — its own bounded, clipped region (flex-1 +
                  min-h-0 + overflow-hidden) so it's the thing that absorbs any
                  slop in the pixel-based cap estimate. The "+N more" button
                  below is a sibling, not a child: giving it `flex-shrink-0`
                  guarantees the flex layout always reserves its full height,
                  so it can never itself get clipped by the cell's
                  overflow-hidden the way it could when it lived inside this
                  same shrinkable stack. */}
              <div className="flex-1 min-h-0 overflow-hidden flex flex-col gap-[3px] min-w-0">
                {visible.map((o, i) => (
                  <OccurrenceChip
                    key={`${o.task.id}-${o.at.getTime()}-${i}`}
                    occ={o}
                    timezone={timezone}
                    onSelect={onSelectTask}
                  />
                ))}
              </div>

              {overflow.length > 0 && (
                <button
                  type="button"
                  onClick={(e) => {
                    e.stopPropagation();
                    setOpenDayKey((k) => (k === cell.key ? null : cell.key));
                  }}
                  className="flex-shrink-0 text-left text-[10px] font-semibold rounded px-1 py-0.5 hover:brightness-110 transition-colors cursor-pointer"
                  style={{ color: "var(--text-secondary)", background: "var(--bg-primary)" }}
                >
                  +{overflow.length} more
                </button>
              )}
            </div>
          );
        })}
      </div>

      {/* Day-agenda modal — Outlook-style "+N more" popup. Rendered once at the
          calendar level (not per-cell) so it's a true centered overlay instead
          of an anchored dropdown that can clip against the grid/viewport edges. */}
      <DayAgendaModal
        dateKey={openDayKey}
        occurrences={openDayKey ? (occurrencesByDay.get(openDayKey) ?? []) : []}
        timezone={timezone}
        onSelect={onSelectTask}
        onClose={() => setOpenDayKey(null)}
      />
    </div>
  );
}

// ---------------------------------------------------------------------------
// Frequent-task legend — a colour key, not a mini-heatmap. Each swatch is one
// densely-firing task (see isFrequentTask), not a day, so it's a horizontal
// row rather than a GitHub-style date grid. Dismissible via local state that
// resets on remount.
// ---------------------------------------------------------------------------

interface FrequentTaskLegendProps {
  tasks: ScheduledTaskWithOwner[];
  dismissed: boolean;
  onDismiss: () => void;
  onSelect: (owner: ScheduledTaskOwner, taskId: string) => void;
}

function FrequentTaskLegend({ tasks, dismissed, onDismiss, onSelect }: FrequentTaskLegendProps) {
  const isDark = useIsDark();
  if (dismissed || tasks.length === 0) return null;

  const visible = tasks.slice(0, MAX_LEGEND_SWATCHES);
  const overflow = tasks.slice(MAX_LEGEND_SWATCHES);

  return (
    <div
      className="flex items-center gap-1.5 h-[32px] px-2 rounded-[8px] border flex-shrink min-w-0"
      style={{ borderColor: "var(--border-primary)", background: "var(--bg-secondary)" }}
    >
      <div className="flex items-center gap-1 min-w-0 overflow-hidden">
        {visible.map((task) => (
          <button
            key={task.id}
            type="button"
            onClick={() => onSelect(task.owner, task.id)}
            title={`${task.owner.name} · ${scheduledTaskDisplayLabel(task)}${task.enabled ? "" : " (disabled)"}`}
            aria-label={`${task.owner.name} · ${scheduledTaskDisplayLabel(task)}`}
            className="flex-shrink-0 w-[12px] h-[12px] rounded-[3px] hover:brightness-110 hover:scale-110 transition-all cursor-pointer"
            style={{ background: frequentTaskColor(task.id, isDark), opacity: task.enabled ? 1 : 0.5 }}
          />
        ))}
        {overflow.length > 0 && (
          <span
            className="text-[10px] font-semibold flex-shrink-0"
            style={{ color: "var(--text-secondary)" }}
            title={overflow.map((t) => `${t.owner.name} · ${scheduledTaskDisplayLabel(t)}`).join(", ")}
          >
            +{overflow.length}
          </span>
        )}
      </div>
      <button
        type="button"
        onClick={onDismiss}
        aria-label="Dismiss frequent-task legend"
        title="These tasks fire too often to show as individual chips — click a square to open one"
        className="flex-shrink-0 flex items-center justify-center w-[18px] h-[18px] rounded-[5px] text-[var(--text-tertiary)] hover:text-[var(--text-primary)] hover:bg-[var(--bg-hover)] transition-colors cursor-pointer"
      >
        <X size={12} />
      </button>
    </div>
  );
}

// ---------------------------------------------------------------------------
// A single frequent-task corner marker inside a day cell's date-number row —
// the per-day counterpart to a FrequentTaskLegend swatch, same colour and
// same click target (task.id-hashed, opens the same edit modal).
// ---------------------------------------------------------------------------

interface CornerMarkerProps {
  task: ScheduledTaskWithOwner;
  onSelect: (owner: ScheduledTaskOwner, taskId: string) => void;
}

function CornerMarker({ task, onSelect }: CornerMarkerProps) {
  const isDark = useIsDark();
  return (
    <button
      type="button"
      onClick={(e) => {
        e.stopPropagation();
        onSelect(task.owner, task.id);
      }}
      title={`${task.owner.name} · ${scheduledTaskDisplayLabel(task)}${task.enabled ? "" : " (disabled)"}`}
      aria-label={`${task.owner.name} · ${scheduledTaskDisplayLabel(task)}`}
      className="flex-shrink-0 w-[7px] h-[7px] rounded-[2px] hover:brightness-110 hover:scale-125 transition-all cursor-pointer"
      style={{ background: frequentTaskColor(task.id, isDark), opacity: task.enabled ? 1 : 0.5 }}
    />
  );
}

// ---------------------------------------------------------------------------
// A single occurrence chip inside a day cell.
// ---------------------------------------------------------------------------

interface OccurrenceChipProps {
  occ: Occurrence;
  timezone: string;
  onSelect: (owner: ScheduledTaskOwner, taskId: string) => void;
}

function OccurrenceChip({ occ, timezone, onSelect }: OccurrenceChipProps) {
  const { task, at } = occ;
  const isDark = useIsDark();
  const dimmed = !task.enabled;
  const time = timeLabelInTz(at, timezone);

  return (
    <button
      type="button"
      onClick={(e) => {
        e.stopPropagation();
        onSelect(task.owner, task.id);
      }}
      title={`${time} · ${task.owner.name} · ${scheduledTaskDisplayLabel(task)}${dimmed ? " (disabled)" : ""}`}
      className="flex items-center gap-1 min-w-0 w-full text-left rounded px-1 py-0 hover:brightness-110 transition-all cursor-pointer"
      style={{
        background: dimmed ? ownerColor(task.owner, isDark) : ownerColorVibrant(task.owner, isDark),
        opacity: dimmed ? 0.65 : 1,
      }}
    >
      <span
        className="text-[10px] font-semibold flex-shrink-0 tabular-nums"
        style={{ color: dimmed ? "var(--text-secondary)" : "#ffffff" }}
      >
        {time}
      </span>
      <span className="text-[10px] truncate min-w-0" style={{ color: dimmed ? "var(--text-primary)" : "#ffffff" }}>
        {scheduledTaskDisplayLabel(task)}
      </span>
    </button>
  );
}

// ---------------------------------------------------------------------------
// Day-agenda modal — listing every occurrence on a day, Outlook-"+N more"-style.
// A centered overlay rather than a cell-anchored dropdown, so it never clips
// against the grid edge or viewport for cells in the last row/column.
// ---------------------------------------------------------------------------

interface DayAgendaModalProps {
  dateKey: string | null;
  occurrences: Occurrence[];
  timezone: string;
  onSelect: (owner: ScheduledTaskOwner, taskId: string) => void;
  onClose: () => void;
}

function DayAgendaModal({ dateKey, occurrences, timezone, onSelect, onClose }: DayAgendaModalProps) {
  const isDark = useIsDark();
  useEffect(() => {
    if (!dateKey) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [dateKey, onClose]);

  let heading = "";
  if (dateKey) {
    const [y, m, d] = dateKey.split("-").map((s) => parseInt(s, 10));
    heading = `${MONTH_LABELS[m - 1]} ${d}, ${y}`;
  }

  return (
    <AnimatePresence>
      {dateKey && (
        <div className="fixed inset-0 z-[300] flex items-center justify-center p-4">
          {/* Backdrop */}
          <motion.div
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            exit={{ opacity: 0 }}
            transition={{ duration: 0.15 }}
            className="absolute inset-0 bg-black/40 backdrop-blur-[1px]"
            onClick={onClose}
          />

          {/* Modal */}
          <motion.div
            initial={{ opacity: 0, scale: 0.96 }}
            animate={{ opacity: 1, scale: 1 }}
            exit={{ opacity: 0, scale: 0.96 }}
            transition={{ duration: 0.15, ease: "easeOut" }}
            className="relative w-full max-w-[560px] max-h-[80vh] flex flex-col rounded-[16px] border border-transparent dark:border-[var(--border-secondary)] overflow-hidden"
            style={{
              background: "var(--bg-secondary)",
              boxShadow: "0 0 0 1px rgba(0,0,0,0.13), 0 18px 48px 0 rgba(0,0,0,0.35)",
            }}
          >
            {/* Header */}
            <div
              className="flex items-center justify-between px-5 py-4 border-b flex-shrink-0"
              style={{ borderColor: "var(--border-secondary)" }}
            >
              <h3 className="text-[17px] font-bold" style={{ color: "var(--text-primary)" }}>
                {heading}
              </h3>
              <button
                type="button"
                onClick={onClose}
                className="p-1.5 rounded-md text-[var(--text-secondary)] hover:bg-[var(--bg-hover)] transition-colors cursor-pointer"
              >
                <X strokeWidth={2} size={18} />
              </button>
            </div>

            {/* Agenda list */}
            <div className="flex-1 overflow-y-auto p-3 flex flex-col gap-[6px]">
              {occurrences.map((o, i) => {
                const dimmed = !o.task.enabled;
                const time = timeLabelInTz(o.at, timezone);
                return (
                  <button
                    key={`${o.task.id}-${o.at.getTime()}-${i}`}
                    type="button"
                    onClick={() => {
                      onSelect(o.task.owner, o.task.id);
                      onClose();
                    }}
                    title={`${time} · ${o.task.owner.name} · ${scheduledTaskDisplayLabel(o.task)}${dimmed ? " (disabled)" : ""}`}
                    className="flex items-center gap-3 min-w-0 w-full text-left rounded-[10px] px-3 py-2 hover:brightness-110 transition-all cursor-pointer"
                    style={{
                      background: dimmed ? ownerColor(o.task.owner, isDark) : ownerColorVibrant(o.task.owner, isDark),
                      opacity: dimmed ? 0.65 : 1,
                    }}
                  >
                    <span
                      className="text-[13px] font-semibold flex-shrink-0 tabular-nums"
                      style={{ color: dimmed ? "var(--text-secondary)" : "#ffffff" }}
                    >
                      {time}
                    </span>
                    <span
                      className="text-[14px] truncate min-w-0"
                      style={{ color: dimmed ? "var(--text-primary)" : "#ffffff" }}
                    >
                      {scheduledTaskDisplayLabel(o.task)}
                    </span>
                  </button>
                );
              })}
            </div>
          </motion.div>
        </div>
      )}
    </AnimatePresence>
  );
}
