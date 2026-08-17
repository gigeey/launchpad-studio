import { create } from "zustand";
import type { TasklistScope } from "../types/api";

export type TriggerRect = {
  left: number;
  top: number;
  width: number;
  height: number;
};

interface TasklistOutputState {
  scope: TasklistScope | null;
  tasklistId: string | null;
  filename: string | null;
  ownerAgentId: string | null;
  triggerRect: TriggerRect | null;
  open: (args: {
    scope: TasklistScope;
    tasklistId: string;
    filename: string;
    ownerAgentId: string | null;
    rect: TriggerRect;
  }) => void;
  close: () => void;
}

export const useTasklistOutputStore = create<TasklistOutputState>((set) => ({
  scope: null,
  tasklistId: null,
  filename: null,
  ownerAgentId: null,
  triggerRect: null,
  open: ({ scope, tasklistId, filename, ownerAgentId, rect }) =>
    set({ scope, tasklistId, filename, ownerAgentId, triggerRect: rect }),
  close: () =>
    set({
      scope: null,
      tasklistId: null,
      filename: null,
      ownerAgentId: null,
      triggerRect: null,
    }),
}));
