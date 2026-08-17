import { create } from "zustand";
import { persist } from "zustand/middleware";
import { Attachment } from "../types/api";

/** Serializable attachment info saved alongside text drafts.
 *
 * A draft may reference attachments that are still uploading (status "uploading")
 * or already committed to the server (status "uploaded" — the default when the
 * discriminator is absent, preserving the legacy shape in persisted storage).
 */
export type DraftAttachment = UploadedDraftAttachment | UploadingDraftAttachment;

export interface UploadedDraftAttachment {
  status?: "uploaded";
  serverId: string;
  attachment: Attachment;
  isFolder: boolean;
  folderPath?: string;
}

/** An attachment that was still uploading when the draft was saved. The File
 *  itself cannot be persisted to localStorage, so it lives in an in-memory
 *  registry in ChatInput keyed by `pendingId`. If the File is unavailable
 *  (e.g. after a page reload), the entry is dropped on restore. */
export interface UploadingDraftAttachment {
  status: "uploading";
  pendingId: string;
  filename: string;
  mimeType: string;
  isFolder: boolean;
  folderPath?: string;
}

interface DraftState {
  drafts: Record<string, string>;
  /** HTML drafts preserving rich content like @mention pills */
  draftHtml: Record<string, string>;
  draftAttachments: Record<string, DraftAttachment[]>;
  setDraft: (agentId: string, text: string, html?: string) => void;
  setDraftAttachments: (agentId: string, attachments: DraftAttachment[]) => void;
  clearDraft: (agentId: string) => void;
}

export const useDraftStore = create<DraftState>()(
  persist(
    (set) => ({
      drafts: {},
      draftHtml: {},
      draftAttachments: {},
      setDraft: (agentId, text, html) =>
        set((state) => ({
          drafts: { ...state.drafts, [agentId]: text },
          draftHtml: html ? { ...state.draftHtml, [agentId]: html } : state.draftHtml,
        })),
      setDraftAttachments: (agentId, attachments) =>
        set((state) => {
          if (attachments.length === 0) {
            const { [agentId]: _, ...rest } = state.draftAttachments;
            return { draftAttachments: rest };
          }
          return {
            draftAttachments: {
              ...state.draftAttachments,
              [agentId]: attachments,
            },
          };
        }),
      clearDraft: (agentId) =>
        set((state) => {
          const { [agentId]: _, ...restDrafts } = state.drafts;
          const { [agentId]: __, ...restHtml } = state.draftHtml;
          const { [agentId]: ___, ...restAttachments } = state.draftAttachments;
          return { drafts: restDrafts, draftHtml: restHtml, draftAttachments: restAttachments };
        }),
    }),
    { name: "chat-drafts" }
  )
);
