import { create } from "zustand";
import type { BookmarkEntry, TranscriptEntry } from "../types/api";
import * as api from "../lib/api";

interface BookmarkState {
  bookmarks: BookmarkEntry[];
  loading: boolean;
  bookmarkedTimestamps: Set<string>;
  targetMessageTs: string | null;
  /** Fallback bookmark entry to render inline when the message isn't in the loaded history */
  targetBookmark: BookmarkEntry | null;
  /** Pinned bookmark to display as an overlay when the message isn't in loaded history */
  pinnedBookmark: { ts: string; content: string; role: string | { agent: string } } | null;

  fetchBookmarks: (agentId: string) => Promise<void>;
  toggleBookmark: (agentId: string, entry: TranscriptEntry) => Promise<void>;
  removeBookmark: (agentId: string, bookmarkId: string) => Promise<void>;
  isBookmarked: (ts: string) => boolean;
  scrollToMessage: (ts: string, bookmark?: BookmarkEntry) => void;
  clearScrollTarget: () => void;
  setPinnedBookmark: (pinned: { ts: string; content: string; role: string | { agent: string } } | null) => void;
  reset: () => void;
}

export const useBookmarkStore = create<BookmarkState>((set, get) => ({
  bookmarks: [],
  loading: false,
  bookmarkedTimestamps: new Set<string>(),
  targetMessageTs: null,
  targetBookmark: null,
  pinnedBookmark: null,

  fetchBookmarks: async (agentId: string) => {
    set({ loading: true });
    try {
      const bookmarks = await api.getBookmarks(agentId);
      const bookmarkedTimestamps = new Set(bookmarks.map((b) => b.message_ts));
      set({ bookmarks, bookmarkedTimestamps });
    } finally {
      set({ loading: false });
    }
  },

  toggleBookmark: async (agentId: string, entry: TranscriptEntry) => {
    const { bookmarks, bookmarkedTimestamps } = get();
    const ts = entry.ts;

    if (bookmarkedTimestamps.has(ts)) {
      // Remove bookmark - optimistic update
      const existing = bookmarks.find((b) => b.message_ts === ts);
      if (!existing) return;

      const newBookmarks = bookmarks.filter((b) => b.id !== existing.id);
      const newTimestamps = new Set(bookmarkedTimestamps);
      newTimestamps.delete(ts);
      set({ bookmarks: newBookmarks, bookmarkedTimestamps: newTimestamps });

      try {
        await api.deleteBookmark(agentId, existing.id);
      } catch {
        // Revert on failure
        set({ bookmarks, bookmarkedTimestamps });
      }
    } else {
      // Add bookmark - optimistic update
      const newTimestamps = new Set(bookmarkedTimestamps);
      newTimestamps.add(ts);
      set({ bookmarkedTimestamps: newTimestamps });

      try {
        const bookmark = await api.addBookmark(
          agentId,
          ts,
          entry.content,
          entry.role,
        );
        set((state) => ({
          bookmarks: [...state.bookmarks, bookmark],
        }));
      } catch {
        // Revert on failure
        const revertTimestamps = new Set(get().bookmarkedTimestamps);
        revertTimestamps.delete(ts);
        set({ bookmarkedTimestamps: revertTimestamps });
      }
    }
  },

  removeBookmark: async (agentId: string, bookmarkId: string) => {
    const { bookmarks, bookmarkedTimestamps } = get();
    const existing = bookmarks.find((b) => b.id === bookmarkId);
    if (!existing) return;

    const newBookmarks = bookmarks.filter((b) => b.id !== bookmarkId);
    const newTimestamps = new Set(bookmarkedTimestamps);
    newTimestamps.delete(existing.message_ts);
    set({ bookmarks: newBookmarks, bookmarkedTimestamps: newTimestamps });

    try {
      await api.deleteBookmark(agentId, bookmarkId);
    } catch {
      // Revert on failure
      set({ bookmarks, bookmarkedTimestamps });
    }
  },

  isBookmarked: (ts: string) => {
    return get().bookmarkedTimestamps.has(ts);
  },

  scrollToMessage: (ts: string, bookmark?: BookmarkEntry) => {
    set({ targetMessageTs: ts, targetBookmark: bookmark ?? null });
  },

  clearScrollTarget: () => {
    set({ targetMessageTs: null, targetBookmark: null });
  },

  setPinnedBookmark: (pinned) => {
    set({ pinnedBookmark: pinned });
  },

  reset: () => {
    set({
      bookmarks: [],
      loading: false,
      bookmarkedTimestamps: new Set<string>(),
      targetMessageTs: null,
      targetBookmark: null,
      pinnedBookmark: null,
    });
  },
}));
