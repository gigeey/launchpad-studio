import { useCallback, useEffect, useRef, useState } from "react";
import { PinOff } from "lucide-react";
import { AssignmentThreadList } from "./AssignmentThreadList";
import { RenameThreadModal } from "./RenameThreadModal";
import { tabLabel } from "./ThreadTabStrip";
import { useUserPreferencesStore } from "../../stores/userPreferencesStore";
import type { AssignmentThreadGroup } from "../../lib/assignmentThreads";
import type { Thread } from "../../types/api";

interface AssignmentsColumnProps {
  /** Assignment threads grouped by `assignment_origin.assignment_id`,
   *  exactly as `resolveAssignmentThreadPartition` (lib/assignmentThreads.ts)
   *  returns them — same source data `AssignmentsTilePanel`'s popover
   *  renders from. */
  assignmentGroups: AssignmentThreadGroup[];
  /** The thread currently shown in the conversation column — marks a row as
   *  "Current". */
  activeThreadId: string;
  /** Switches the active thread. */
  onSelectThread: (threadId: string) => void;
  /** Archive an assignment thread — same non-destructive "hide, don't
   *  delete" semantics as the popover's row `X` (see `AssignmentThreadList`'s
   *  doc comment). */
  onArchiveThread: (threadId: string) => void;
  /** Rename an assignment thread — `title` is `null` to clear back to the
   *  assignment-derived placeholder. */
  onRenameThread: (threadId: string, title: string | null) => Promise<unknown>;
  /** Thread id -> "is actively streaming right now" — passed straight
   *  through to `AssignmentThreadList` (see its own doc comment on this
   *  prop). */
  streamingByThreadId?: Record<string, boolean>;
  /** Unpins the column — reverts to the popover-only experience. There is no
   *  other way back to the popover flow once pinned, so this is the one
   *  control that closes the column for good (until pinned again from the
   *  popover). */
  onUnpin: () => void;
}

/** Min/max drag bounds for the resize handle — mirrors `ChannelsColumn`'s own
 *  clamp exactly, so the two pinned columns feel identical to resize. */
const MIN_WIDTH_PX = 220;
const MAX_WIDTH_PX = 560;

/** Persistent, pinned counterpart to `AssignmentsTilePanel`'s floating
 *  popover — rendered as its own column to the left of the chat area in
 *  `ChatView` once the operator pins the popover (see
 *  `AssignmentsTilePanel`'s "Pin" button). Shows the exact same list
 *  (`AssignmentThreadList`) so switching an assignment run-thread never
 *  again costs the "open the popover, then click a row" two-click round
 *  trip — the list just stays visible. Owns its own rename-modal instance
 *  (mirroring `ThreadTabStrip`'s `renameTarget`) rather than sharing one
 *  with the tab strip, since this column can be pinned open while the
 *  strip's popover is closed. Mirrors `ChannelsColumn` one-for-one, reading
 *  `assignmentsColumnWidth`/`setAssignmentsColumnWidth` instead of the
 *  channel column's own width fields. */
export function AssignmentsColumn({
  assignmentGroups,
  activeThreadId,
  onSelectThread,
  onArchiveThread,
  onRenameThread,
  streamingByThreadId,
  onUnpin,
}: AssignmentsColumnProps) {
  const [renameTarget, setRenameTarget] = useState<Thread | null>(null);

  const columnWidth = useUserPreferencesStore((s) => s.assignmentsColumnWidth);
  const setColumnWidth = useUserPreferencesStore((s) => s.setAssignmentsColumnWidth);
  const columnRef = useRef<HTMLDivElement>(null);
  const resizingRef = useRef(false);
  // Column's own left edge, captured once per drag — stable for its
  // duration since only this column's width changes, not its position, so
  // re-reading the DOM on every `mousemove` would be redundant work.
  const dragStartLeftRef = useRef(0);
  const [isResizing, setIsResizing] = useState(false);

  const startResizing = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    if (columnRef.current) {
      dragStartLeftRef.current = columnRef.current.getBoundingClientRect().left;
    }
    resizingRef.current = true;
    setIsResizing(true);
  }, []);

  const stopResizing = useCallback(() => {
    if (resizingRef.current && columnRef.current) {
      setColumnWidth(columnRef.current.offsetWidth);
    }
    resizingRef.current = false;
    setIsResizing(false);
  }, [setColumnWidth]);

  const resize = useCallback((e: MouseEvent) => {
    if (!resizingRef.current || !columnRef.current) return;
    const newWidth = e.clientX - dragStartLeftRef.current;
    if (newWidth >= MIN_WIDTH_PX && newWidth <= MAX_WIDTH_PX) {
      // Direct DOM write during the drag — no React re-render per pixel.
      columnRef.current.style.width = `${newWidth}px`;
    }
  }, []);

  useEffect(() => {
    if (!isResizing) return;
    window.addEventListener("mousemove", resize);
    window.addEventListener("mouseup", stopResizing);
    document.body.style.cursor = "col-resize";
    return () => {
      window.removeEventListener("mousemove", resize);
      window.removeEventListener("mouseup", stopResizing);
      document.body.style.cursor = "default";
    };
  }, [isResizing, resize, stopResizing]);

  return (
    <div
      ref={columnRef}
      className="flex flex-col min-h-0 shrink-0 relative border-r border-[var(--border-secondary)]"
      style={{ width: columnWidth }}
    >
      <div className="flex items-center justify-between px-[16px] py-[12px] min-h-[65px] border-b border-[var(--border-secondary)] shrink-0">
        <span className="text-[14px] font-bold text-[var(--text-primary)]">Assignments</span>
        <button
          type="button"
          onClick={onUnpin}
          data-testid="assignments-column-unpin"
          aria-label="Unpin assignments list"
          title="Unpin assignments list"
          className="w-[28px] h-[28px] rounded-[8px] flex items-center justify-center text-[var(--text-secondary)] hover:bg-[var(--bg-hover)] hover:text-[var(--text-primary)] transition-colors cursor-pointer"
        >
          <PinOff className="w-[14px] h-[14px]" />
        </button>
      </div>

      <AssignmentThreadList
        assignmentGroups={assignmentGroups}
        activeThreadId={activeThreadId}
        onSelectThread={onSelectThread}
        onOpenRename={setRenameTarget}
        onArchiveThread={onArchiveThread}
        streamingByThreadId={streamingByThreadId}
      />

      {renameTarget && (
        <RenameThreadModal
          open
          initialValue={renameTarget.title ?? ""}
          placeholder={tabLabel(renameTarget)}
          onClose={() => setRenameTarget(null)}
          onSubmit={async (title) => { await onRenameThread(renameTarget.id, title); }}
        />
      )}

      {/* Resize handle — column is left-anchored, so this sits on its right
         edge (mirrors `ChannelsColumn`'s own handle). */}
      <div
        className="absolute right-[-4px] top-0 bottom-0 w-[8px] cursor-col-resize z-30 group"
        onMouseDown={startResizing}
      >
        <div className="absolute inset-y-0 left-1/2 w-[1px] bg-transparent group-hover:bg-[var(--accent)] transition-colors" />
      </div>
    </div>
  );
}
