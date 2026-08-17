import { create } from "zustand";

interface CompetenciesModalState {
  agentId: string | null;
  /** The opening thread's focus path, snapshotted at open time — drives the
   *  Skills tab's "Project skills" section. `null` when the thread has no
   *  focus path set. */
  focusPath: string | null;
  open: (agentId: string, focusPath?: string | null) => void;
  close: () => void;
}

export const useCompetenciesModalStore = create<CompetenciesModalState>((set) => ({
  agentId: null,
  focusPath: null,
  open: (agentId, focusPath = null) => set({ agentId, focusPath }),
  close: () => set({ agentId: null, focusPath: null }),
}));
