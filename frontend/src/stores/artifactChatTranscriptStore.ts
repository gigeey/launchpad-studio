import { create } from "zustand";

export interface ArtifactChatMessage {
  role: "user" | "assistant";
  content: string;
  /** ISO timestamp — present for entries hydrated from the server's durable
   *  transcript (`GET .../artifacts/{id}/chat`, which always carries a real
   *  `ts`). Absent for an optimistic bubble appended locally before the round
   *  trip confirms it; `ArtifactChatPanel`'s render adapter fabricates one in
   *  that case so bubble ordering/time-of-day display never break. Optional
   *  (rather than always-fabricated here) so this store stays a plain mirror
   *  of what the panel actually knows, not a place that invents data. */
  ts?: string;
}

interface ArtifactChatTranscriptState {
  /** Keyed the same way as `stores/draftStore.ts`'s per-artifact composer
   *  drafts (`artifact:{artifactId}`, see `artifactChatDraftKey` in
   *  `ArtifactChatPanel.tsx`). In-memory runtime state only — the backend
   *  chat transcript (`GET .../artifacts/{id}/chat`) is the durable source
   *  of truth. This store just lets a mini-thread's transcript survive
   *  navigating away from the Assets view and back within the same running
   *  session, and lets a just-sent bubble paint instantly. */
  transcripts: Record<string, ArtifactChatMessage[]>;
  setTranscript: (key: string, messages: ArtifactChatMessage[]) => void;
}

export const useArtifactChatTranscriptStore = create<ArtifactChatTranscriptState>()((set) => ({
  transcripts: {},
  setTranscript: (key, messages) =>
    set((state) => ({
      transcripts: { ...state.transcripts, [key]: messages },
    })),
}));
