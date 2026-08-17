import { useState } from "react";
import { ChevronRight, ClipboardList, Hash, Inbox, X } from "lucide-react";
import { ThreadStreamingBadge, ThreadUnreadDot } from "../shared/ThreadActivityBadge";
import type { AssignmentThreadGroup } from "../../lib/assignmentThreads";
import type { Thread } from "../../types/api";

interface AssignmentThreadListProps {
  /** Assignment threads grouped by `assignment_origin.assignment_id`,
   *  exactly as `resolveAssignmentThreadPartition` (lib/assignmentThreads.ts)
   *  returns them — group order and per-group thread order are the hook's
   *  own, not re-derived here. */
  assignmentGroups: AssignmentThreadGroup[];
  /** The thread currently shown in the conversation column — marks a row as
   *  "Current". */
  activeThreadId: string;
  /** Switches the active thread. Deliberately does NOT close/hide whatever
   *  surface this list is rendered inside — switching conversations
   *  shouldn't kick the operator out of a list they might still be
   *  browsing. */
  onSelectThread: (threadId: string) => void;
  /** Right-click on a row opens the rename modal — the caller owns the
   *  actual modal instance (see `ThreadTabStrip`'s `renameTarget` and
   *  `AssignmentsColumn`'s own, identically-shaped, state). */
  onOpenRename: (thread: Thread) => void;
  /** Archive an assignment thread — the row's hover-revealed `X`, same
   *  non-destructive "hide, don't delete" semantics as an ordinary pill's
   *  close button. Once archived, `resolveAssignmentThreadPartition` drops
   *  the thread from `assignmentGroups` entirely, so the row disappears from
   *  this list on the next render. */
  onArchiveThread: (threadId: string) => void;
  /** Thread id -> "is actively streaming right now", from
   *  `resolveAssignmentStreamingByThreadId` (lib/assignmentThreads.ts).
   *  Outranks the plain unread dot below (mirrors `resolveThreadActivity`'s
   *  streaming > unread priority) since a run still in flight is more
   *  actionable-looking than a stale "you haven't looked at this" marker.
   *  Optional so this list keeps rendering (just without the live badge) for
   *  any caller that hasn't been taught about it yet. */
  streamingByThreadId?: Record<string, boolean>;
}

/** Shared "browse assignment run-threads" list — one group header per
 *  assignment plus its rows, an empty state when there are no
 *  assignment-originated conversations yet. Used both by
 *  `AssignmentsTilePanel` (the Assignments tile's floating popover) and
 *  `AssignmentsColumn` (the persistent, pinned version of the same list) so
 *  the two surfaces render identical content — pinning/unpinning only
 *  changes the surrounding chrome, never the list itself. Mirrors
 *  `ChannelThreadList` one-for-one; the one structural difference is the
 *  group key (`assignmentId`, a dynamic id resolved to a name via
 *  `resolveAssignmentThreadPartition`'s `assignmentLookup`) versus
 *  `ChannelThreadList`'s static `kind` enum, so every group here shares the
 *  same `ClipboardList` glyph instead of a per-kind icon map. */
export function AssignmentThreadList({
  assignmentGroups,
  activeThreadId,
  onSelectThread,
  onOpenRename,
  onArchiveThread,
  streamingByThreadId,
}: AssignmentThreadListProps) {
  // Per-assignment collapse — local rather than persisted, same reasoning as
  // `ChannelThreadList`'s `collapsedKinds`: this list is reused by both the
  // floating popover and the pinned column, and a fresh "everything
  // expanded" state on each mount is the least surprising default for
  // either surface.
  const [collapsedAssignmentIds, setCollapsedAssignmentIds] = useState<Set<string>>(new Set());
  const toggleGroup = (assignmentId: string) => {
    setCollapsedAssignmentIds((prev) => {
      const next = new Set(prev);
      if (next.has(assignmentId)) next.delete(assignmentId); else next.add(assignmentId);
      return next;
    });
  };

  if (assignmentGroups.length === 0) {
    return (
      <div className="flex-1 min-h-0 flex flex-col items-center justify-center gap-[6px] py-[28px] text-[var(--modal-text-tertiary)] text-[12px]">
        <Inbox className="w-[20px] h-[20px] opacity-50" />
        No assignment runs yet
      </div>
    );
  }

  return (
    <div className="flex-1 min-h-0 overflow-y-auto overscroll-contain pb-[4px]">
      {assignmentGroups.map((group) => {
        const isCollapsed = collapsedAssignmentIds.has(group.assignmentId);
        return (
          <div key={group.assignmentId} data-testid={`assignments-tile-group-${group.assignmentId}`}>
            <div
              role="button"
              tabIndex={0}
              onClick={() => toggleGroup(group.assignmentId)}
              onKeyDown={(e) => {
                if (e.key === "Enter" || e.key === " ") {
                  e.preventDefault();
                  toggleGroup(group.assignmentId);
                }
              }}
              aria-expanded={!isCollapsed}
              data-testid={`assignments-tile-group-toggle-${group.assignmentId}`}
              title={group.label}
              className="group/hdr flex items-center gap-[6px] px-[12px] pt-[10px] pb-[4px] cursor-pointer select-none min-w-0 bg-[var(--modal-bg-tertiary)] border-b border-[var(--modal-border-secondary)]"
            >
              {/* Section header shows the assignment glyph at rest and morphs
                  it into the collapse chevron on hover/focus — never both at
                  once. Both share one fixed-size slot so swapping them never
                  nudges the label. */}
              <span className="relative flex items-center justify-center w-[12px] h-[12px] shrink-0">
                <ClipboardList className="w-[11px] h-[11px] text-[var(--modal-text-tertiary)] transition-opacity group-hover/hdr:opacity-0 group-focus-visible/hdr:opacity-0" />
                <ChevronRight
                  className={`absolute inset-0 m-auto w-[10px] h-[10px] text-[var(--modal-text-tertiary)] opacity-0 transition-all group-hover/hdr:opacity-100 group-focus-visible/hdr:opacity-100 ${isCollapsed ? "" : "rotate-90"}`}
                />
              </span>

              <span className="flex-1 min-w-0 truncate text-[11px] font-bold uppercase tracking-wide text-[var(--modal-text-primary)]">
                {group.label}
              </span>
            </div>
            {!isCollapsed && group.threads.map((info) => {
              const isActive = info.thread.id === activeThreadId;
              return (
                // `group/row` wrapper, not a single <button> — the
                // hover-revealed archive `X` below needs to be a sibling hit
                // target (a button can't nest inside a button), same
                // select/close split ThreadTabStrip's own pills use.
                <div
                  key={info.thread.id}
                  className={`group/row flex items-center border-b border-[var(--modal-border-secondary)] transition-colors ${isActive
                    ? "bg-[var(--modal-sidebar-active-bg)]"
                    : "hover:bg-[var(--modal-accent)]/12"
                    }`}
                >
                  <button
                    type="button"
                    onClick={() => onSelectThread(info.thread.id)}
                    onContextMenu={(e) => {
                      e.preventDefault();
                      onOpenRename(info.thread);
                    }}
                    data-testid={`assignments-tile-row-${info.thread.id}`}
                    title={info.label}
                    className="flex-1 min-w-0 flex items-center gap-[8px] pl-[12px] pr-[4px] py-[7px] text-left cursor-pointer"
                  >
                    <Hash className={`w-[13px] h-[13px] shrink-0 ${isActive ? "text-[var(--modal-sidebar-active-text-primary)]" : "text-[var(--modal-text-tertiary)]"}`} />
                    <span className={`flex-1 min-w-0 truncate text-[13px] ${isActive ? "text-[var(--modal-sidebar-active-text-primary)] font-medium" : "text-[var(--modal-text-primary)]"}`}>
                      {info.label}
                    </span>
                    {isActive ? (
                      <span className="shrink-0 text-[10px] font-medium text-[var(--modal-sidebar-active-text-secondary)]">Current</span>
                    ) : streamingByThreadId?.[info.thread.id] ? (
                      <ThreadStreamingBadge id={info.thread.id} />
                    ) : info.unread ? (
                      <ThreadUnreadDot id={info.thread.id} />
                    ) : null}
                  </button>
                  {/* Archive = same non-destructive "hide, don't delete"
                      semantics as an ordinary pill's `X` — hover-revealed
                      rather than always-on so the row's resting state stays
                      uncluttered. */}
                  <button
                    type="button"
                    onClick={(e) => {
                      e.stopPropagation();
                      onArchiveThread(info.thread.id);
                    }}
                    data-testid={`assignments-tile-archive-${info.thread.id}`}
                    aria-label="Archive thread"
                    className="shrink-0 mr-[8px] w-[20px] h-[20px] flex items-center justify-center rounded-[6px]
                             text-[var(--modal-text-tertiary)] opacity-0 group-hover/row:opacity-100
                             hover:bg-[var(--modal-bg-hover)] hover:text-[var(--modal-text-primary)]
                             transition-colors cursor-pointer"
                  >
                    <X className="w-[12px] h-[12px]" />
                  </button>
                </div>
              );
            })}
          </div>
        );
      })}
    </div>
  );
}
