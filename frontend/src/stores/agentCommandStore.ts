import { create } from "zustand";
import { AgentCommand, getAgentCommands } from "../lib/api";

interface AgentCommandState {
  /** Cache: agent command type (e.g. "claude") → discovered commands */
  commandsByAgent: Record<string, AgentCommand[]>;
  loading: boolean;

  /** Fetch commands for a CLI agent type. Results are cached per-session. */
  fetchCommands: (command: string, workingDir?: string | null) => Promise<void>;

  /** Clear the entire cache (e.g. on workflow refresh). */
  clearCache: () => void;
}

export const useAgentCommandStore = create<AgentCommandState>((set, get) => ({
  commandsByAgent: {},
  loading: false,

  fetchCommands: async (command, workingDir) => {
    // Return cached if available
    if (get().commandsByAgent[command]) return;

    set({ loading: true });
    try {
      const commands = await getAgentCommands(command, workingDir);
      set((state) => ({
        commandsByAgent: { ...state.commandsByAgent, [command]: commands },
        loading: false,
      }));
    } catch (err) {
      console.warn("Failed to fetch agent commands:", err);
      // Cache empty array so we don't retry on every popover open
      set((state) => ({
        commandsByAgent: { ...state.commandsByAgent, [command]: [] },
        loading: false,
      }));
    }
  },

  clearCache: () => set({ commandsByAgent: {} }),
}));
