import { useEffect, useLayoutEffect, useRef, useState, type RefObject } from "react";
import { createPortal } from "react-dom";
import { AnimatePresence, motion } from "framer-motion";
import { Pin, X } from "lucide-react";
import { ChannelThreadList } from "./ChannelThreadList";
import type { ChannelThreadGroup } from "../../lib/channelThreads";
import type { Thread } from "../../types/api";

/** Matches the panel's own `w-[...]` below — kept as a constant instead of
 *  reading it back off the DOM because position has to be computed BEFORE
 *  the (not-yet-positioned) panel has ever painted. Mirrors
 *  `ThreadOverflowPanel`'s own `PANEL_WIDTH_PX` constant. */
const PANEL_WIDTH_PX = 260;
/** Gap between the Channels tile and the panel's top edge. */
const ANCHOR_GAP_PX = 8;
/** Never let the panel touch the actual edge of the window — same
 *  clearance `ThreadOverflowPanel`/ChatInput's attach menu use. */
const VIEWPORT_MARGIN_PX = 8;

/** Left-aligns the panel under its anchor tile, clamped against both edges
 *  of the window. Left-aligned (not right-aligned like
 *  `ThreadOverflowPanel`'s "More" pill) because the Channels tile sits near
 *  the LEFT of the strip, right next to Main — right-aligning it there would
 *  run the panel back over Main and the tile itself instead of opening into
 *  the open space to its right. */
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

interface ChannelsTilePanelProps {
  open: boolean;
  onClose: () => void;
  /** The Channels tile's own wrapper — used both to compute this panel's
   *  `position: fixed` coordinates (see `computePosition` above) and, in the
   *  outside-click listener below, to tell "clicked the tile again" apart
   *  from a genuine click elsewhere. */
  anchorRef: RefObject<HTMLElement | null>;
  /** Channel threads grouped by `channel_origin.kind`, exactly as
   *  `resolveChannelThreadPartition` (lib/channelThreads.ts) returns them —
   *  group order and per-group thread order are the hook's own, not
   *  re-derived here. */
  channelGroups: ChannelThreadGroup[];
  /** The thread currently shown in the conversation column — marks a row as
   *  "Current" the same way `ThreadOverflowPanel`'s Active tab does. */
  activeThreadId: string;
  /** Switches the active thread — reuses the tab strip's own
   *  `onSelectThread`, exactly like every other row-click in this strip
   *  (pills, the More panel). Deliberately does NOT close this panel, same
   *  reasoning as `ThreadOverflowPanel`: switching conversations shouldn't
   *  kick the operator out of a list they might still be browsing. */
  onSelectThread: (threadId: string) => void;
  /** Right-click on a row opens the rename modal — reuses the tab strip's
   *  own `renameTarget` state/modal instance (see `ThreadTabStrip`'s
   *  `handleContextMenu` for the pill equivalent) rather than this
   *  presentation component owning a second one. */
  onOpenRename: (thread: Thread) => void;
  /** Archive a channel thread — the row's hover-revealed `X`, same
   *  non-destructive "hide, don't delete" semantics as an ordinary pill's
   *  close button (reuses the tab strip's own `onArchiveThread` prop
   *  directly, see `ThreadTabStrip`'s doc comment on that prop). Once
   *  archived, `resolveChannelThreadPartition` drops the thread from
   *  `channelGroups` entirely (see that function's own doc comment) so the
   *  row disappears from this panel on the next render — there is no
   *  "Archived channels" tab to recover it from yet. */
  onArchiveThread: (threadId: string) => void;
  /** Whether the persistent Channels column (`ChannelsColumn`, rendered
   *  alongside the chat area in `ChatView`) is currently pinned open for this
   *  agent — hides the "Pin" control below once true, since pinning again
   *  would be a no-op; unpinning happens from the column's own header
   *  instead, not from here. */
  pinned: boolean;
  /** Pins the persistent Channels column open and closes this popover — the
   *  same list keeps rendering, just embedded in the page instead of
   *  floating, so the popover would otherwise be redundant right after. */
  onPin: () => void;
}

/** Floating "browse channel conversations" panel for the tab strip's
 *  collapsed Channels tile — the expanded view moves channel threads
 *  behind. Reuses
 *  `ThreadOverflowPanel`'s portal/positioning/dismissal recipe (portaled to
 *  `document.body` so the chat column's `overflow-hidden` ancestor can never
 *  clip it; closes on outside click, Escape, or its own close button) but
 *  drops the tabs/checkboxes/bulk-delete machinery that panel needs for
 *  ordinary threads — a channel conversation is browsed, selected, (via
 *  right-click) renamed, and (via the row's own hover-revealed `X`) archived
 *  from here; there's no per-row delete yet, same as an ordinary thread pill,
 *  and no "Archived channels" recovery tab yet either (see `onArchiveThread`
 *  below). */
export function ChannelsTilePanel({
  open,
  onClose,
  anchorRef,
  channelGroups,
  activeThreadId,
  onSelectThread,
  onOpenRename,
  onArchiveThread,
  pinned,
  onPin,
}: ChannelsTilePanelProps) {
  const panelRef = useRef<HTMLDivElement>(null);
  // Viewport-relative coordinates for the portaled panel below — null until
  // the first open, then left holding its last value through the exit
  // animation rather than resetting to null on close, so AnimatePresence's
  // exit tween plays from wherever the panel actually was instead of
  // snapping away. Same approach as `ThreadOverflowPanel`.
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
          aria-label="Channels"
          data-testid="channels-tile-panel"
          initial={{ opacity: 0, y: -8, scale: 0.97 }}
          animate={{ opacity: 1, y: 0, scale: 1 }}
          exit={{ opacity: 0, y: -8, scale: 0.97 }}
          transition={{ duration: 0.14, ease: "easeOut" }}
          // Same "elevated surface" recipe as `ThreadOverflowPanel` — see
          // that component's doc comment for the reasoning behind the
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
            <span className="text-[16px] font-bold text-[var(--modal-text-primary)]">Channels</span>
            <div className="flex items-center gap-[2px]">
              {/* Pins the same list open as a persistent column next to the
                  chat area (`ChannelsColumn`) instead of this floating
                  popover — saves the "reopen the popover, then click a row"
                  two-click round trip on every subsequent thread switch.
                  Hidden once already pinned; unpinning happens from the
                  column's own header instead. */}
              {!pinned && (
                <button
                  type="button"
                  onClick={onPin}
                  data-testid="channels-tile-panel-pin"
                  aria-label="Pin channels list"
                  title="Pin channels list"
                  className="w-[22px] h-[22px] rounded-[6px] flex items-center justify-center text-[var(--modal-text-secondary)]
                           hover:bg-[var(--modal-bg-hover)] hover:text-[var(--modal-text-primary)] transition-colors cursor-pointer"
                >
                  <Pin className="w-[13px] h-[13px]" />
                </button>
              )}
              <button
                type="button"
                onClick={onClose}
                data-testid="channels-tile-panel-close"
                aria-label="Close"
                className="w-[22px] h-[22px] rounded-[6px] flex items-center justify-center text-[var(--modal-text-secondary)]
                         hover:bg-[var(--modal-bg-hover)] hover:text-[var(--modal-text-primary)] transition-colors cursor-pointer"
              >
                <X className="w-[13px] h-[13px]" />
              </button>
            </div>
          </div>

          <ChannelThreadList
            channelGroups={channelGroups}
            activeThreadId={activeThreadId}
            onSelectThread={onSelectThread}
            onOpenRename={onOpenRename}
            onArchiveThread={onArchiveThread}
          />
        </motion.div>
      )}
    </AnimatePresence>,
    document.body,
  );
}
