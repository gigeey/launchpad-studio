import { useEffect, useLayoutEffect, useRef, useState, type RefObject } from "react";
import { createPortal } from "react-dom";
import { AnimatePresence, motion } from "framer-motion";
import { Pin, X } from "lucide-react";
import { AssignmentThreadList } from "./AssignmentThreadList";
import type { AssignmentThreadGroup } from "../../lib/assignmentThreads";
import type { Thread } from "../../types/api";

/** Matches the panel's own `w-[...]` below — kept as a constant instead of
 *  reading it back off the DOM because position has to be computed BEFORE
 *  the (not-yet-positioned) panel has ever painted. Mirrors
 *  `ChannelsTilePanel`'s own `PANEL_WIDTH_PX` constant. */
const PANEL_WIDTH_PX = 260;
/** Gap between the Assignments tile and the panel's top edge. */
const ANCHOR_GAP_PX = 8;
/** Never let the panel touch the actual edge of the window — same
 *  clearance `ChannelsTilePanel`/`ThreadOverflowPanel` use. */
const VIEWPORT_MARGIN_PX = 8;

/** Left-aligns the panel under its anchor tile, clamped against both edges
 *  of the window — identical positioning rule to `ChannelsTilePanel`'s own
 *  `computePosition`, since the Assignments tile sits directly beside the
 *  Channels tile in the strip. */
function computePosition(anchor: HTMLElement): { top: number; left: number } {
  const rect = anchor.getBoundingClientRect();
  let left = rect.left;
  if (left + PANEL_WIDTH_PX > window.innerWidth - VIEWPORT_MARGIN_PX) {
    left = window.innerWidth - PANEL_WIDTH_PX - VIEWPORT_MARGIN_PX;
  }
  if (left < VIEWPORT_MARGIN_PX) {
    left = VIEWPORT_MARGIN_PX;
  }
  return { top: rect.bottom + ANCHOR_GAP_PX, left };
}

interface AssignmentsTilePanelProps {
  open: boolean;
  onClose: () => void;
  /** The Assignments tile's own wrapper — used both to compute this panel's
   *  `position: fixed` coordinates (see `computePosition` above) and, in the
   *  outside-click listener below, to tell "clicked the tile again" apart
   *  from a genuine click elsewhere. */
  anchorRef: RefObject<HTMLElement | null>;
  /** Assignment threads grouped by `assignment_origin.assignment_id`,
   *  exactly as `resolveAssignmentThreadPartition` (lib/assignmentThreads.ts)
   *  returns them — group order and per-group thread order are the hook's
   *  own, not re-derived here. */
  assignmentGroups: AssignmentThreadGroup[];
  /** The thread currently shown in the conversation column — marks a row as
   *  "Current" the same way `ThreadOverflowPanel`'s Active tab does. */
  activeThreadId: string;
  /** Switches the active thread — reuses the tab strip's own
   *  `onSelectThread`, exactly like every other row-click in this strip
   *  (pills, the More panel). Deliberately does NOT close this panel, same
   *  reasoning as `ChannelsTilePanel`: switching conversations shouldn't
   *  kick the operator out of a list they might still be browsing. */
  onSelectThread: (threadId: string) => void;
  /** Right-click on a row opens the rename modal — reuses the tab strip's
   *  own `renameTarget` state/modal instance (see `ThreadTabStrip`'s
   *  `handleContextMenu` for the pill equivalent) rather than this
   *  presentation component owning a second one. */
  onOpenRename: (thread: Thread) => void;
  /** Archive an assignment thread — the row's hover-revealed `X`, same
   *  non-destructive "hide, don't delete" semantics as an ordinary pill's
   *  close button (reuses the tab strip's own `onArchiveThread` prop
   *  directly, see `ThreadTabStrip`'s doc comment on that prop). Once
   *  archived, `resolveAssignmentThreadPartition` drops the thread from
   *  `assignmentGroups` entirely (see that function's own doc comment) so
   *  the row disappears from this panel on the next render — there is no
   *  "Archived assignments" tab to recover it from yet. */
  onArchiveThread: (threadId: string) => void;
  /** Thread id -> "is actively streaming right now" — passed straight
   *  through to `AssignmentThreadList` (see its own doc comment on this
   *  prop). */
  streamingByThreadId?: Record<string, boolean>;
  /** Whether the persistent Assignments column (`AssignmentsColumn`,
   *  rendered alongside the chat area in `ChatView`) is currently pinned
   *  open for this agent — hides the "Pin" control below once true, since
   *  pinning again would be a no-op; unpinning happens from the column's own
   *  header instead, not from here. */
  pinned: boolean;
  /** Pins the persistent Assignments column open and closes this popover —
   *  the same list keeps rendering, just embedded in the page instead of
   *  floating, so the popover would otherwise be redundant right after. */
  onPin: () => void;
}

/** Floating "browse assignment run-threads" panel for the tab strip's
 *  collapsed Assignments tile — mirrors `ChannelsTilePanel` one-for-one,
 *  just sourced from `assignmentGroups` (assignmentThreads.ts) instead of
 *  channel groups. Reuses `ThreadOverflowPanel`'s portal/positioning/
 *  dismissal recipe (portaled to `document.body` so the chat column's
 *  `overflow-hidden` ancestor can never clip it; closes on outside click,
 *  Escape, or its own close button) — an assignment run-thread is browsed,
 *  selected, (via right-click) renamed, and (via the row's own
 *  hover-revealed `X`) archived from here; there's no per-row delete yet,
 *  same as an ordinary thread pill, and no "Archived assignments" recovery
 *  tab yet either (see `onArchiveThread` below). */
export function AssignmentsTilePanel({
  open,
  onClose,
  anchorRef,
  assignmentGroups,
  activeThreadId,
  onSelectThread,
  onOpenRename,
  onArchiveThread,
  streamingByThreadId,
  pinned,
  onPin,
}: AssignmentsTilePanelProps) {
  const panelRef = useRef<HTMLDivElement>(null);
  // Viewport-relative coordinates for the portaled panel below — null until
  // the first open, then left holding its last value through the exit
  // animation rather than resetting to null on close, so AnimatePresence's
  // exit tween plays from wherever the panel actually was instead of
  // snapping away. Same approach as `ChannelsTilePanel`.
  const [pos, setPos] = useState<{ top: number; left: number } | null>(null);

  useLayoutEffect(() => {
    if (!open) return;
    const anchor = anchorRef.current;
    if (!anchor) return;
    const recompute = () => setPos(computePosition(anchor));
    recompute();
    window.addEventListener("resize", recompute);
    return () => window.removeEventListener("resize", recompute);
  }, [open, anchorRef]);

  useEffect(() => {
    if (!open) return;
    const handleClick = (e: MouseEvent) => {
      if (
        panelRef.current && !panelRef.current.contains(e.target as Node) &&
        anchorRef.current && !anchorRef.current.contains(e.target as Node)
      ) {
        onClose();
      }
    };
    const handleKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    document.addEventListener("mousedown", handleClick);
    document.addEventListener("keydown", handleKey);
    return () => {
      document.removeEventListener("mousedown", handleClick);
      document.removeEventListener("keydown", handleKey);
    };
  }, [open, onClose, anchorRef]);

  return createPortal(
    <AnimatePresence>
      {open && pos && (
        <motion.div
          ref={panelRef}
          role="dialog"
          aria-label="Assignments"
          data-testid="assignments-tile-panel"
          initial={{ opacity: 0, y: -8, scale: 0.97 }}
          animate={{ opacity: 1, y: 0, scale: 1 }}
          exit={{ opacity: 0, y: -8, scale: 0.97 }}
          transition={{ duration: 0.14, ease: "easeOut" }}
          // Same "elevated surface" recipe as `ChannelsTilePanel` — see that
          // component's doc comment for the reasoning behind the
          // `--modal-*` namespace and the hairline-shadow-as-border trick.
          className="fixed z-[60] w-[260px] max-h-[360px] flex flex-col
                   rounded-[16px] border border-transparent dark:border-[var(--modal-border-secondary)]
                   bg-[var(--modal-bg)] overflow-hidden"
          style={{
            top: pos.top,
            left: pos.left,
            transformOrigin: "top left",
            boxShadow: "0 0 0 1px rgba(0,0,0,0.1), 0 4px 12px rgba(0,0,0,0.08), 0 12px 28px rgba(0,0,0,0.12)",
          }}
        >
          <div className="flex items-center justify-between px-[14px] py-[12px] shrink-0 border-b border-[var(--modal-border-secondary)]">
            <span className="text-[16px] font-bold text-[var(--modal-text-primary)]">Assignments</span>
            <div className="flex items-center gap-[2px]">
              {/* Pins the same list open as a persistent column next to the
                  chat area (`AssignmentsColumn`) instead of this floating
                  popover — saves the "reopen the popover, then click a row"
                  two-click round trip on every subsequent thread switch.
                  Hidden once already pinned; unpinning happens from the
                  column's own header instead. */}
              {!pinned && (
                <button
                  type="button"
                  onClick={onPin}
                  data-testid="assignments-tile-panel-pin"
                  aria-label="Pin assignments list"
                  title="Pin assignments list"
                  className="w-[22px] h-[22px] rounded-[6px] flex items-center justify-center text-[var(--modal-text-secondary)]
                           hover:bg-[var(--modal-bg-hover)] hover:text-[var(--modal-text-primary)] transition-colors cursor-pointer"
                >
                  <Pin className="w-[13px] h-[13px]" />
                </button>
              )}
              <button
                type="button"
                onClick={onClose}
                data-testid="assignments-tile-panel-close"
                aria-label="Close"
                className="w-[22px] h-[22px] rounded-[6px] flex items-center justify-center text-[var(--modal-text-secondary)]
                         hover:bg-[var(--modal-bg-hover)] hover:text-[var(--modal-text-primary)] transition-colors cursor-pointer"
              >
                <X className="w-[13px] h-[13px]" />
              </button>
            </div>
          </div>

          <AssignmentThreadList
            assignmentGroups={assignmentGroups}
            activeThreadId={activeThreadId}
            onSelectThread={onSelectThread}
            onOpenRename={onOpenRename}
            onArchiveThread={onArchiveThread}
            streamingByThreadId={streamingByThreadId}
          />
        </motion.div>
      )}
    </AnimatePresence>,
    document.body,
  );
}
