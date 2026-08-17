import { useEffect } from "react";
import { Bookmark, Loader2, Trash2 } from "lucide-react";
import { useBookmarkStore } from "../../stores/bookmarkStore";
import type { BookmarkEntry } from "../../types/api";

/** Format an ISO timestamp as HH:MM. */
function formatTime(isoString: string): string {
  const date = new Date(isoString);
  return date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

/** Return display name for the message role. */
function roleName(role: string | { agent: string } | { task_id: string }, agentName?: string): string {
  if (typeof role === "string") {
    return role === "user" ? "You" : role;
  }
  if ("task_id" in role) {
    return "Scheduled Item";
  }
  return agentName ?? role.agent;
}

interface BookmarkPanelProps {
  agentId: string;
  agentName?: string;
}

export function BookmarkPanel({ agentId, agentName }: BookmarkPanelProps) {
  const bookmarks = useBookmarkStore((s) => s.bookmarks);
  const loading = useBookmarkStore((s) => s.loading);
  const removeBookmark = useBookmarkStore((s) => s.removeBookmark);
  const fetchBookmarks = useBookmarkStore((s) => s.fetchBookmarks);
  const scrollToMessage = useBookmarkStore((s) => s.scrollToMessage);

  useEffect(() => {
    fetchBookmarks(agentId);
  }, [agentId, fetchBookmarks]);

  // Sort by message_ts descending (newest first)
  const sorted = [...bookmarks].sort(
    (a, b) => new Date(b.message_ts).getTime() - new Date(a.message_ts).getTime(),
  );

  return (
    <div className="flex flex-col h-full overflow-hidden">
      {/* Header */}
      <div className="px-[16px] pt-[16px] pb-[8px]">
        <div className="flex items-center gap-[8px]">
          <span className="text-[14px] font-semibold text-[var(--text-primary)]">
            Bookmarks
          </span>
          {bookmarks.length > 0 && (
            <span className="text-[11px] font-bold text-[var(--text-secondary)] bg-[var(--bg-hover)] px-[6px] py-[1px] rounded-[4px]">
              {bookmarks.length}
            </span>
          )}
        </div>
      </div>

      {/* Bookmark list */}
      <div className="flex-1 overflow-y-auto px-[16px] py-[8px] custom-scrollbar">
        {loading ? (
          <div className="flex items-center justify-center py-[48px]">
            <Loader2 className="w-[20px] h-[20px] text-[var(--text-secondary)] animate-spin" />
          </div>
        ) : bookmarks.length === 0 ? (
          <div className="py-[48px] text-center text-[13px] text-[var(--text-secondary)] leading-relaxed flex flex-col items-center gap-3">
            <Bookmark className="w-[48px] h-[48px] text-[var(--text-tertiary)]" />
            <span>No bookmarks yet</span>
            <span className="text-[11px] text-[var(--text-tertiary)]">
              Hover over a message and click the<br />
              bookmark icon to save it here
            </span>
          </div>
        ) : (
          <div className="flex flex-col gap-[8px]">
            {sorted.map((bookmark) => (
              <BookmarkItem
                key={bookmark.id}
                bookmark={bookmark}
                agentName={agentName}
                onDelete={() => removeBookmark(agentId, bookmark.id)}
                onScrollTo={() => scrollToMessage(bookmark.message_ts, bookmark)}
              />
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

function BookmarkItem({
  bookmark,
  agentName,
  onDelete,
  onScrollTo,
}: {
  bookmark: BookmarkEntry;
  agentName?: string;
  onDelete: () => void;
  onScrollTo: () => void;
}) {
  const preview =
    bookmark.message_content.length > 150
      ? bookmark.message_content.slice(0, 150) + "…"
      : bookmark.message_content;

  return (
    <div
      className="group flex items-start gap-[8px] p-[12px] rounded-[12px] bg-[var(--bg-tertiary)] hover:bg-[var(--bg-hover)] transition-all cursor-pointer"
      onClick={onScrollTo}
    >
      <div className="flex-1 min-w-0">
        <div className="flex items-center gap-[6px] mb-[4px]">
          <span className="text-[12px] font-semibold text-[var(--text-primary)]">
            {roleName(bookmark.message_role, agentName)}
          </span>
          <span className="text-[11px] text-[var(--text-tertiary)]">
            {formatTime(bookmark.message_ts)}
          </span>
        </div>
        <p className="text-[13px] text-[var(--text-primary)] leading-relaxed whitespace-pre-wrap break-words">
          {preview}
        </p>
      </div>
      <button
        onClick={(e) => {
          e.stopPropagation();
          onDelete();
        }}
        className="flex-shrink-0 w-[24px] h-[24px] rounded-[6px] flex items-center justify-center text-[var(--text-tertiary)] opacity-0 group-hover:opacity-100 hover:text-[var(--error)] transition-all cursor-pointer"
        aria-label="Delete bookmark"
      >
        <Trash2 className="w-[18px] h-[18px]" />
      </button>
    </div>
  );
}
