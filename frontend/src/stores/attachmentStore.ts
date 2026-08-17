import { create } from "zustand";
import type { Attachment } from "../types/api";
import * as api from "../lib/api";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface CachedAttachment {
  attachment: Attachment;
  thumbnailUrl: string | null;
  status: "loading" | "loaded" | "error";
  loadedAt: number;
  agentId: string;
}

const MAX_CACHED_ATTACHMENTS = 200;
const DEFAULT_MAX_AGE_MS = 30 * 60 * 1000; // 30 minutes

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/** Custom fetcher for loading attachment info from non-agent contexts (e.g. teams). */
export interface AttachmentFetcher {
  getInfo: (attachmentId: string) => Promise<Attachment>;
  getUrl: (attachmentId: string) => string;
}

interface AttachmentStoreState {
  attachmentCache: Map<string, CachedAttachment>;

  // Actions
  loadAttachment: (agentId: string, attachmentId: string) => Promise<void>;
  loadAttachmentWithFetcher: (entityId: string, attachmentId: string, fetcher: AttachmentFetcher) => Promise<void>;
  getFromCache: (attachmentId: string) => CachedAttachment | null;
  evictOldEntries: (maxAge?: number) => void;
  clearCacheForAgent: (agentId: string) => void;
}

export const useAttachmentStore = create<AttachmentStoreState>()((set, get) => ({
  attachmentCache: new Map(),

  loadAttachment: async (agentId: string, attachmentId: string) => {
    const existing = get().attachmentCache.get(attachmentId);
    if (existing && existing.status !== "error") return;

    // Set loading status
    set((state) => {
      const next = new Map(state.attachmentCache);
      next.set(attachmentId, {
        attachment: existing?.attachment ?? ({} as Attachment),
        thumbnailUrl: null,
        status: "loading",
        loadedAt: Date.now(),
        agentId,
      });
      return { attachmentCache: next };
    });

    try {
      const attachment = await api.getAttachmentInfo(agentId, attachmentId);
      const isImage = attachment.attachment_type === "image";
      const thumbnailUrl = isImage
        ? api.getAttachmentUrl(agentId, attachmentId)
        : null;

      set((state) => {
        const next = new Map(state.attachmentCache);
        next.set(attachmentId, {
          attachment,
          thumbnailUrl,
          status: "loaded",
          loadedAt: Date.now(),
          agentId,
        });

        // LRU eviction: if over max, remove oldest entries
        if (next.size > MAX_CACHED_ATTACHMENTS) {
          const entries = [...next.entries()].sort(
            (a, b) => a[1].loadedAt - b[1].loadedAt,
          );
          const toRemove = entries.slice(0, next.size - MAX_CACHED_ATTACHMENTS);
          for (const [key] of toRemove) {
            next.delete(key);
          }
        }

        return { attachmentCache: next };
      });
    } catch {
      set((state) => {
        const next = new Map(state.attachmentCache);
        const current = next.get(attachmentId);
        if (current) {
          next.set(attachmentId, { ...current, status: "error" });
        }
        return { attachmentCache: next };
      });
    }
  },

  loadAttachmentWithFetcher: async (entityId: string, attachmentId: string, fetcher: AttachmentFetcher) => {
    const existing = get().attachmentCache.get(attachmentId);
    if (existing && existing.status !== "error") return;

    // Set loading status
    set((state) => {
      const next = new Map(state.attachmentCache);
      next.set(attachmentId, {
        attachment: existing?.attachment ?? ({} as Attachment),
        thumbnailUrl: null,
        status: "loading",
        loadedAt: Date.now(),
        agentId: entityId,
      });
      return { attachmentCache: next };
    });

    try {
      const attachment = await fetcher.getInfo(attachmentId);
      const isImage = attachment.attachment_type === "image";
      const thumbnailUrl = isImage ? fetcher.getUrl(attachmentId) : null;

      set((state) => {
        const next = new Map(state.attachmentCache);
        next.set(attachmentId, {
          attachment,
          thumbnailUrl,
          status: "loaded",
          loadedAt: Date.now(),
          agentId: entityId,
        });

        if (next.size > MAX_CACHED_ATTACHMENTS) {
          const entries = [...next.entries()].sort(
            (a, b) => a[1].loadedAt - b[1].loadedAt,
          );
          const toRemove = entries.slice(0, next.size - MAX_CACHED_ATTACHMENTS);
          for (const [key] of toRemove) {
            next.delete(key);
          }
        }

        return { attachmentCache: next };
      });
    } catch {
      set((state) => {
        const next = new Map(state.attachmentCache);
        const current = next.get(attachmentId);
        if (current) {
          next.set(attachmentId, { ...current, status: "error" });
        }
        return { attachmentCache: next };
      });
    }
  },

  getFromCache: (attachmentId: string) => {
    return get().attachmentCache.get(attachmentId) ?? null;
  },

  evictOldEntries: (maxAge: number = DEFAULT_MAX_AGE_MS) => {
    const now = Date.now();
    set((state) => {
      const next = new Map(state.attachmentCache);
      let changed = false;
      for (const [key, entry] of next) {
        if (now - entry.loadedAt > maxAge) {
          next.delete(key);
          changed = true;
        }
      }
      return changed ? { attachmentCache: next } : state;
    });
  },

  clearCacheForAgent: (agentId: string) => {
    set((state) => {
      const next = new Map(state.attachmentCache);
      let changed = false;
      for (const [key, entry] of next) {
        if (entry.agentId === agentId) {
          next.delete(key);
          changed = true;
        }
      }
      return changed ? { attachmentCache: next } : state;
    });
  },
}));
