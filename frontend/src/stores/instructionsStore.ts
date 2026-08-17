import { create } from "zustand";
import * as api from "../lib/api";
import type { Instruction } from "../lib/api";

interface InstructionsState {
  agentId: string | null;
  instructions: Instruction[];
  loading: boolean;
  error: string | null;

  // Global (cross-agent) filename pattern list driving the backend scanner.
  filenames: string[];
  filenamesLoading: boolean;
  filenamesError: string | null;

  load: (agentId: string) => Promise<void>;
  setEnabled: (id: string, enabled: boolean) => Promise<void>;
  loadFilenames: () => Promise<void>;
  setFilenames: (list: string[]) => Promise<string[]>;
  reset: () => void;
}

function errorMessage(err: unknown): string {
  if (err instanceof Error) return err.message;
  return String(err);
}

export const useInstructionsStore = create<InstructionsState>((set, get) => ({
  agentId: null,
  instructions: [],
  loading: false,
  error: null,

  filenames: [],
  filenamesLoading: false,
  filenamesError: null,

  load: async (agentId: string) => {
    set({ agentId, loading: true, error: null });
    try {
      const instructions = await api.listInstructions(agentId);
      if (get().agentId !== agentId) return;
      set({ instructions, loading: false });
    } catch (err) {
      if (get().agentId !== agentId) return;
      set({ loading: false, error: errorMessage(err) });
    }
  },

  setEnabled: async (id: string, enabled: boolean) => {
    const { agentId, instructions } = get();
    if (!agentId) return;
    const previous = instructions;
    set({
      instructions: instructions.map((i) => (i.id === id ? { ...i, enabled } : i)),
      error: null,
    });
    try {
      const updated = await api.patchInstruction(agentId, id, enabled);
      if (get().agentId !== agentId) return;
      set((state) => ({
        instructions: state.instructions.map((i) => (i.id === id ? updated : i)),
      }));
    } catch (err) {
      if (get().agentId !== agentId) return;
      set({ instructions: previous, error: errorMessage(err) });
    }
  },

  loadFilenames: async () => {
    set({ filenamesLoading: true, filenamesError: null });
    try {
      const filenames = await api.getInstructionFilenames();
      set({ filenames, filenamesLoading: false });
    } catch (err) {
      set({ filenamesLoading: false, filenamesError: errorMessage(err) });
    }
  },

  setFilenames: async (list: string[]) => {
    const previous = get().filenames;
    set({ filenames: list, filenamesError: null });
    try {
      const normalized = await api.setInstructionFilenames(list);
      set({ filenames: normalized });
      return normalized;
    } catch (err) {
      set({ filenames: previous, filenamesError: errorMessage(err) });
      throw err;
    }
  },

  reset: () => {
    set({
      agentId: null,
      instructions: [],
      loading: false,
      error: null,
    });
  },
}));
