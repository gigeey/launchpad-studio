import { useCallback, useEffect, useRef, useState } from "react";
import { PinOff } from "lucide-react";
import { ChannelThreadList } from "./ChannelThreadList";
import { RenameThreadModal } from "./RenameThreadModal";
import { tabLabel } from "./ThreadTabStrip";
import { useUserPreferencesStore } from "../../stores/userPreferencesStore";
import type { ChannelThreadGroup } from "../../lib/channelThreads";
import type { Thread } from "../../types/api";

interface ChannelsColumnProps {
  /** Channel threads grouped by `channel_origin.kind`, exactly as
   *  `resolveChannelThreadPartition` (lib/channelThreads.ts) returns them —
   *  same source data `ChannelsTilePanel`'s popover renders from. */
  channelGroups: ChannelThreadGroup[];
  /** The thread currently shown in the conversation column — marks a row as
   *  "Current". */
  activeThreadId: string;
  /** Switches the active thread. */
  onSelectThread: (threadId: string) => void;
  /** Archive a channel thread — same non-destructive "hide, don't delete"
   *  semantics as the popover's row `X` (see `ChannelThreadList`'s doc
   *  comment). */
  onArchiveThread: (threadId: string) => void;
  /** Rename a channel thread — `title` is `null` to clear back to the
   *  channel-derived placeholder. */
  onRenameThread: (threadId: string, title: string | null) => Promise<unknown>;
  /** Unpins the column — reverts to the popover-only experience. There is no
   *  other way back to the popover flow once pinned, so this is the one
   *  control that closes the column for good (until pinned again from the
   *  popover). */
  onUnpin: () => void;
}

/** Min/max drag bounds for the resize handle — mirrors the memory/bookmark
 *  side panel's own clamp (see ChatView's `resize`), just narrower on the
 *  low end since this column has less content per row than that panel. */
const MIN_WIDTH_PX = 220;
const MAX_WIDTH_PX = 560;

/** Persistent, pinned counterpart to `ChannelsTilePanel`'s floating popover —
 *  rendered as its own column to the left of the chat area in `ChatView` once
 *  the operator pins the popover (see `ChannelsTilePanel`'s "Pin" button). Shows
 *  the exact same list (`ChannelThreadList`) so switching a channel thread
 *  never again costs the "open the popover, then click a row" two-click
 *  round trip — the list just stays visible. Owns its own rename-modal
 *  instance (mirroring `ThreadTabStrip`'s `renameTarget`) rather than
 *  sharing one with the tab strip, since this column can be pinned open
 *  while the strip's popover is closed. */
export function ChannelsColumn({
  channelGroups,
  activeThreadId,
  onSelectThread,
  onArchiveThread,
  onRenameThread,
  onUnpin,
}: ChannelsColumnProps) {
  const [renameTarget, setRenameTarget] = useState<Thread | null>(null);

  const columnWidth = useUserPreferencesStore((s) => s.channelsColumnWidth);
  const setColumnWidth = useUserPreferencesStore((s) => s.setChannelsColumnWidth);
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
        <span className="text-[14px] font-bold text-[var(--text-primary)]">Channels</span>
        <button
          type="button"
          onClick={onUnpin}
          data-testid="channels-column-unpin"
          aria-label="Unpin channels list"
          title="Unpin channels list"
          className="w-[28px] h-[28px] rounded-[8px] flex items-center justify-center text-[var(--text-secondary)] hover:bg-[var(--bg-hover)] hover:text-[var(--text-primary)] transition-colors cursor-pointer"
        >
          <PinOff className="w-[14px] h-[14px]" />
        </button>
      </div>

      <ChannelThreadList
        channelGroups={channelGroups}
        activeThreadId={activeThreadId}
        onSelectThread={onSelectThread}
        onOpenRename={setRenameTarget}
        onArchiveThread={onArchiveThread}
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
         edge (mirrors the memory/bookmark side panel's own handle, which
         sits on that panel's left edge since it's right-anchored). */}
      <div
        className="absolute right-[-4px] top-0 bottom-0 w-[8px] cursor-col-resize z-30 group"
        onMouseDown={startResizing}
      >
        <div className="absolute inset-y-0 left-1/2 w-[1px] bg-transparent group-hover:bg-[var(--accent)] transition-colors" />
      </div>
    </div>
  );
}
