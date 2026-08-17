import { create } from "zustand";

interface AgentProfileModalState {
    /** null = closed, "new" = create mode, string = edit mode (agentId) */
    mode: null | "new" | string;
    openNew: () => void;
    openEdit: (agentId: string) => void;
    close: () => void;
}

export const useAgentProfileModalStore = create<AgentProfileModalState>()((set) => ({
    mode: null,
    openNew: () => set({ mode: "new" }),
    openEdit: (agentId: string) => set({ mode: agentId }),
    close: () => set({ mode: null }),
}));
