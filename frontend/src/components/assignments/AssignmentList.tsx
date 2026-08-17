import { useMemo, useState, type MouseEvent } from "react";
import { Calendar, CalendarClock, CalendarX, Clock, Loader2, Pause, Play, Repeat, Webhook } from "lucide-react";
import type { AssignmentOwner, AssignmentWithOwner } from "../../hooks/useAssignments";
import { assignmentDisplayLabel } from "../../hooks/useAssignments";
import { assignmentTriggerLabel } from "./assignmentAdapters";
import { ScheduledTaskOwnerChip } from "../scheduled/ScheduledTaskOwnerChip";
import { useUserPreferencesStore } from "../../stores/userPreferencesStore";
import { AssignmentLivenessBadge } from "./AssignmentLivenessBadge";

// ---------------------------------------------------------------------------
// Flat, scrollable list view for the Assignments page — the alternate to the
// calendar month grid, and the ONLY place non-schedule assignments (webhook /
// future connector-event triggers) surface, since those have no fire series to
// plot on a calendar. Every assignment appears here regardless of trigger,
// each row footered with a plain-language "when this fires" label
// (cron cadence for schedules, "When POSTed to …" for webhooks).
//
// Card chrome + the All/Active/Inactive filter + Group-by-agent toggle mirror
// the legacy ScheduledTask list's design language from before the Assignments
// convergence (see .scheduled-list-tile in App.css).
// ---------------------------------------------------------------------------

type AssignmentFilter = "all" | "active" | "inactive";

const FILTER_OPTIONS: { key: AssignmentFilter; label: string }[] = [
  { key: "all", label: "All" },
  { key: "active", label: "Active" },
  { key: "inactive", label: "Inactive" },
];

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

type AssignmentStatus = "active" | "paused" | "expired";

/** An assignment past its `expires_at` reads as "Expired" rather than the
 *  generic "Paused", even in the brief window before the scheduler has
 *  gotten around to flipping `enabled` to false — so this checks the
 *  timestamp directly instead of trusting `enabled` alone. */
function assignmentStatus(assignment: AssignmentWithOwner): AssignmentStatus {
  if (assignment.expires_at && new Date(assignment.expires_at).getTime() <= Date.now()) {
    return "expired";
  }
  return assignment.enabled ? "active" : "paused";
}

/** Cron assignments carry a next-fire instant and sort ascending by it; every
 *  other trigger (no scheduled fire) sorts after them. */
function sortByNextFire(assignments: AssignmentWithOwner[]): AssignmentWithOwner[] {
  return [...assignments].sort((a, b) => {
    if (!a.next_fire_at && !b.next_fire_at) return 0;
    if (!a.next_fire_at) return 1;
    if (!b.next_fire_at) return -1;
    return new Date(a.next_fire_at).getTime() - new Date(b.next_fire_at).getTime();
  });
}

interface OwnerGroup {
  owner: AssignmentOwner;
  assignments: AssignmentWithOwner[];
}

/** Buckets already-sorted assignments by owner, then orders the buckets
 *  alphabetically by owner name for a stable, predictable group order. */
function groupByOwner(assignments: AssignmentWithOwner[]): OwnerGroup[] {
  const groups = new Map<string, OwnerGroup>();
  for (const a of assignments) {
    let group = groups.get(a.owner.id);
    if (!group) {
      group = { owner: a.owner, assignments: [] };
      groups.set(a.owner.id, group);
    }
    group.assignments.push(a);
  }
  return [...groups.values()].sort((x, y) =>
    x.owner.name.localeCompare(y.owner.name, undefined, { sensitivity: "base" }),
  );
}

interface AssignmentListProps {
  assignments: AssignmentWithOwner[];
  onSelect: (owner: AssignmentOwner, assignmentId: string) => void;
  /** Fires the assignment immediately from its own tile (the discoverable
   *  counterpart to the editor modal's "Run now"). The owner supplies the
   *  agent to navigate into; the caller owns the actual
   *  trigger→switch-thread→navigate sequence (see `AssignmentsView`), so this
   *  list stays presentational — exactly like `onSelect` above. Optional so a
   *  caller that only wants a read-only list can omit the per-tile action
   *  entirely. */
  onRunNow?: (owner: AssignmentOwner, assignmentId: string) => void | Promise<void>;
  loading?: boolean;
}

export function AssignmentList({ assignments, onSelect, onRunNow, loading = false }: AssignmentListProps) {
  const [filter, setFilter] = useState<AssignmentFilter>("all");
  const groupByAgent = useUserPreferencesStore((s) => s.scheduledListGroupByAgent);
  const setGroupByAgent = useUserPreferencesStore((s) => s.setScheduledListGroupByAgent);

  const sorted = useMemo(() => sortByNextFire(assignments), [assignments]);
  const filtered = useMemo(() => {
    if (filter === "all") return sorted;
    return sorted.filter((a) => (filter === "active" ? a.enabled : !a.enabled));
  }, [sorted, filter]);
  const groups = useMemo(
    () => (groupByAgent ? groupByOwner(filtered) : []),
    [filtered, groupByAgent],
  );

  if (loading) {
    return (
      <div className="flex items-center justify-center h-full bg-[var(--bg-secondary)]">
        <Loader2 size={28} className="animate-spin text-[var(--text-secondary)]" />
      </div>
    );
  }

  if (assignments.length === 0) {
    return (
      <div className="flex flex-col items-center justify-center h-full gap-2 text-[var(--text-secondary)] bg-[var(--bg-secondary)]">
        <Calendar size={48} className="opacity-40" />
        <p className="text-[15px] font-medium">No assignments</p>
      </div>
    );
  }

  return (
    <div className="h-full overflow-y-auto bg-[var(--bg-secondary)]">
      <div className="max-w-[1440px] mx-auto px-0 py-4">
        <div className="mb-4 flex items-center justify-between gap-3 px-4">
          <AssignmentFilterTabs filter={filter} onChange={setFilter} />
          <GroupByAgentToggle checked={groupByAgent} onChange={setGroupByAgent} />
        </div>

        {filtered.length === 0 ? (
          <div className="flex flex-col items-center justify-center gap-2 text-[var(--text-secondary)] py-16">
            <Calendar size={40} className="opacity-40" />
            <p className="text-[14px] font-medium">No {filter} assignments</p>
          </div>
        ) : groupByAgent ? (
          <div className="flex flex-col gap-6 ">
            {groups.map((group) => (
              <section key={group.owner.id} data-testid="assignment-list-group">
                <div className="flex items-center justify-between gap-2 mb-3 px-3 py-2 rounded-[0px] bg-[var(--bg-tertiary)] border-t border-b border-[var(--border-secondary)] dark:border-[#333333]">
                  <ScheduledTaskOwnerChip
                    owner={group.owner}
                    size="md"
                    nameClassName="text-[16px] leading-[20px] font-semibold"
                  />
                  <span className="flex items-center justify-center min-w-[20px] h-[20px] px-1.5 rounded-full bg-[var(--bg-secondary)] text-[12px] text-[var(--text-tertiary)]">
                    {group.assignments.length}
                  </span>
                </div>
                <div className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-[10px] items-start px-4">
                  {group.assignments.map((a) => (
                    <AssignmentListRow key={a.id} assignment={a} onSelect={onSelect} onRunNow={onRunNow} />
                  ))}
                </div>
              </section>
            ))}
          </div>
        ) : (
          <div
            className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-[10px] items-start px-4"
            data-testid="assignment-list-flat"
          >
            {filtered.map((a) => (
              <AssignmentListRow key={a.id} assignment={a} onSelect={onSelect} onRunNow={onRunNow} />
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Filter tabs — All / Active / Inactive segmented control.
// ---------------------------------------------------------------------------

interface AssignmentFilterTabsProps {
  filter: AssignmentFilter;
  onChange: (filter: AssignmentFilter) => void;
}

function AssignmentFilterTabs({ filter, onChange }: AssignmentFilterTabsProps) {
  return (
    <div className="inline-flex items-center gap-1 p-[3px] rounded-[10px] border border-[var(--border-secondary)] bg-[var(--bg-secondary)]">
      {FILTER_OPTIONS.map((opt) => (
        <button
          key={opt.key}
          type="button"
          data-testid={`assignment-list-filter-${opt.key}`}
          aria-pressed={filter === opt.key}
          onClick={() => onChange(opt.key)}
          className={`flex items-center justify-center h-[26px] px-3 rounded-[8px] text-[13px] font-medium transition-colors cursor-pointer ${filter === opt.key
            ? "bg-[var(--sidebar-active-bg)] text-[var(--sidebar-active-text-primary)]"
            : "text-[var(--text-secondary)] hover:text-[var(--text-primary)]"
            }`}
        >
          {opt.label}
        </button>
      ))}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Group-by-agent toggle.
// ---------------------------------------------------------------------------

interface GroupByAgentToggleProps {
  checked: boolean;
  onChange: (checked: boolean) => void;
}

function GroupByAgentToggle({ checked, onChange }: GroupByAgentToggleProps) {
  return (
    <label className="flex items-center gap-2 text-[13px] font-medium text-[var(--text-secondary)] cursor-pointer select-none flex-shrink-0">
      Group by agent
      <button
        type="button"
        role="switch"
        aria-checked={checked}
        aria-label="Group by agent"
        data-testid="assignment-list-group-toggle"
        onClick={() => onChange(!checked)}
        className={`relative w-[32px] h-[18px] rounded-full transition-colors cursor-pointer ${checked ? "bg-[var(--accent)]" : "bg-[var(--border-primary)]"
          }`}
      >
        <div
          className={`absolute top-[2px] w-[14px] h-[14px] rounded-full bg-white shadow transition-transform ${checked ? "translate-x-[16px]" : "translate-x-[2px]"
            }`}
        />
      </button>
    </label>
  );
}

// ---------------------------------------------------------------------------
// Row
// ---------------------------------------------------------------------------

interface AssignmentListRowProps {
  assignment: AssignmentWithOwner;
  onSelect: (owner: AssignmentOwner, assignmentId: string) => void;
  onRunNow?: (owner: AssignmentOwner, assignmentId: string) => void | Promise<void>;
}

/** Trigger glyph for the footer: recurring/one-time clock for schedules, a
 *  webhook glyph for webhook triggers, a generic calendar-clock otherwise. */
function TriggerIcon({ assignment }: { assignment: AssignmentWithOwner }) {
  if (assignment.trigger.type === "Cron") {
    return assignment.trigger.is_recurring ? (
      <Repeat className="w-[12px] h-[12px] flex-shrink-0" />
    ) : (
      <Clock className="w-[12px] h-[12px] flex-shrink-0" />
    );
  }
  if (assignment.trigger.type === "Webhook") {
    return <Webhook className="w-[12px] h-[12px] flex-shrink-0" />;
  }
  return <CalendarClock className="w-[12px] h-[12px] flex-shrink-0" />;
}

function AssignmentListRow({ assignment, onSelect, onRunNow }: AssignmentListRowProps) {
  const triggerLabel = assignmentTriggerLabel(assignment);
  const nextLabel = formatNextFire(assignment.next_fire_at);
  const showNextFire = assignment.trigger.type === "Cron" && !!assignment.next_fire_at;
  const status = assignmentStatus(assignment);

  // In-flight guard for the per-tile "Run now" — mirrors the editor modal's
  // own `triggering` flag so a double-click can't fire the assignment twice
  // while the first trigger request is still round-tripping.
  const [triggering, setTriggering] = useState(false);
  // Same disable rule as the modal's "Run now": a disabled assignment can't be
  // fired manually either. Also blocks re-entry while a trigger is in flight.
  const runNowDisabled = !assignment.enabled || triggering;

  const handleRunNow = async (e: MouseEvent<HTMLButtonElement>) => {
    // Keep the click on the action button from also opening the editor via the
    // surrounding tile's own click handler.
    e.stopPropagation();
    if (!onRunNow || runNowDisabled) return;
    setTriggering(true);
    try {
      await onRunNow(assignment.owner, assignment.id);
    } catch {
      // Surfacing the failure is the editor's job (it has an error line); from
      // a bare tile we just release the spinner so the action stays clickable.
    } finally {
      setTriggering(false);
    }
  };

  return (
    // A tile carrying its own nested action button can't itself be a
    // <button> (no button-in-button), so it's a keyboard-activable role=button
    // <div> — same select/action split the assignment run-thread rows use.
    <div
      role="button"
      tabIndex={0}
      data-testid="assignment-list-row"
      onClick={() => onSelect(assignment.owner, assignment.id)}
      onKeyDown={(e) => {
        // Only the tile itself opens the editor on Enter/Space — a key press
        // while the nested "Run now" button is focused belongs to that button.
        if (e.target !== e.currentTarget) return;
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          onSelect(assignment.owner, assignment.id);
        }
      }}
      className="group/tile scheduled-list-tile w-full text-left rounded-xl border border-[var(--border-secondary)] dark:border-[#333333] bg-[var(--bg-tertiary)] px-4 py-[14px] flex flex-col gap-2 cursor-pointer hover:border-[var(--accent)]/40"
    >
      <div className="flex items-start justify-between gap-3">
        <h4
          className={`flex-1 min-h-[38px] text-[15px] font-semibold leading-[19px] line-clamp-2 break-words ${status === "active" ? "text-[var(--text-primary)]" : "text-[var(--text-tertiary)]"
            }`}
        >
          {assignmentDisplayLabel(assignment)}
        </h4>
        <div className="flex-shrink-0 flex items-center gap-[6px]">
          {onRunNow && (
            // Hover-revealed "Run now" — the discoverable, in-place twin of the
            // editor modal's footer button, reusing the identical
            // trigger→open-thread→navigate handler (owned by the caller). Shown
            // but disabled when the assignment is paused, same as the modal.
            <button
              type="button"
              data-testid="assignment-list-row-run-now"
              onClick={handleRunNow}
              onKeyDown={(e) => e.stopPropagation()}
              disabled={runNowDisabled}
              aria-label="Run now"
              title={assignment.enabled ? "Run now" : "Enable this assignment to run it now"}
              className={`flex items-center justify-center w-[26px] h-[26px] rounded-md border border-[var(--border-secondary)] transition-all ${runNowDisabled
                ? "opacity-0 group-hover/tile:opacity-40 cursor-not-allowed text-[var(--text-tertiary)]"
                : "opacity-0 group-hover/tile:opacity-100 focus-visible:opacity-100 cursor-pointer text-[var(--text-secondary)] hover:text-[var(--accent)] hover:border-[var(--accent)]/40"
                }`}
            >
              {triggering ? <Loader2 size={13} className="animate-spin" /> : <Play size={13} />}
            </button>
          )}
          <span
            title={status === "active" ? "Active" : status === "expired" ? "Expired" : "Paused"}
            data-testid="assignment-list-row-status"
            className={`flex items-center justify-center w-[26px] h-[26px] rounded-md border border-[var(--border-secondary)] ${status === "active" ? "text-[var(--success)]" : status === "expired" ? "text-[var(--error)]" : "text-[var(--text-tertiary)]"
              }`}
          >
            {status === "active" ? <Play size={13} /> : status === "expired" ? <CalendarX size={13} /> : <Pause size={13} />}
          </span>
        </div>
      </div>

      <ScheduledTaskOwnerChip owner={assignment.owner} size="md" />

      <div className="border-t border-[var(--border-secondary)] -mx-4" />

      <div
        className="flex items-center gap-[6px] text-[12px] text-[var(--text-tertiary)] min-w-0"
        title={showNextFire ? `Next: ${nextLabel}` : triggerLabel}
      >
        <TriggerIcon assignment={assignment} />
        <span className="truncate">{triggerLabel}</span>
        {showNextFire && (
          <>
            <span className="opacity-50">·</span>
            <span className="truncate">{nextLabel}</span>
          </>
        )}
      </div>

      {/* Every trigger kind except Webhook now carries liveness data — see
          `AssignmentLivenessBadge`'s own doc for why Webhook is excluded. */}
      {assignment.trigger.type !== "Webhook" && (
        <AssignmentLivenessBadge health={assignment.watch_health} triggerType={assignment.trigger.type} />
      )}
    </div>
  );
}
