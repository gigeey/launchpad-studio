import { create } from "zustand";

interface TaskCreateModalState {
    workflowId: string | null;
    open: (workflowId: string) => void;
    close: () => void;
}

export const useTaskCreateModalStore = create<TaskCreateModalState>((set) => ({
    workflowId: null,
    open: (workflowId) => set({ workflowId }),
    close: () => set({ workflowId: null }),
}));
