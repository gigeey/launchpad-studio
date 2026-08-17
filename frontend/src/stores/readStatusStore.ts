import { create } from "zustand";
import { persist } from "zustand/middleware";

interface ReadStatusState {
  /** agentId → timestamp (ms) of when the user last viewed the chat */
  lastRead: Record<string, number>;
  markRead: (agentId: string, readAt?: string | number) => void;
  isUnread: (agentId: string, lastActivityAt: string | null) => boolean;
}

export const useReadStatusStore = create<ReadStatusState>()(
  persist(
    (set, get) => ({
      lastRead: {},

      markRead: (agentId, readAt) => {
        const ts =
          typeof readAt === "number"
            ? readAt
            : typeof readAt === "string"
              ? new Date(readAt).getTime()
              : Date.now();

        if (!Number.isFinite(ts)) return;

        set((state) => ({
          lastRead: {
            ...state.lastRead,
            [agentId]: Math.max(state.lastRead[agentId] ?? 0, ts),
          },
        }));
      },

      isUnread: (agentId, lastActivityAt) => {
        if (!lastActivityAt) return false;
        const lastReadTs = get().lastRead[agentId];
        if (!lastReadTs) return true; // never opened → unread
        return new Date(lastActivityAt).getTime() > lastReadTs;
      },
    }),
    { name: "read-status" }
  )
);
