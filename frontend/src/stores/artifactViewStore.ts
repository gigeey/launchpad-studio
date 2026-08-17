import { create } from "zustand";

// Store-driven half of the artifact renderer's "props OR store" reuse (PRD
// mirroring `tasklistOutputStore`'s relationship to
// `TasklistOutputPreview`/`TasklistOutputPortal`). A mount point that already
// holds `agentId`/`artifactId` in local state (e.g. a modal that owns the
// selection) should drive `ArtifactPreview` directly via props instead of
// going through this store.

interface ArtifactViewState {
  agentId: string | null;
  artifactId: string | null;
  open: (args: { agentId: string; artifactId: string }) => void;
  close: () => void;
}

export const useArtifactViewStore = create<ArtifactViewState>((set) => ({
  agentId: null,
  artifactId: null,
  open: ({ agentId, artifactId }) => set({ agentId, artifactId }),
  close: () => set({ agentId: null, artifactId: null }),
}));
