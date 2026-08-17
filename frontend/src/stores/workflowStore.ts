import { create } from "zustand";
import type {
  WorkflowSummary,
  TaskSnapshot,
  TaskSummary,
} from "../types/workflow";
import * as api from "../lib/api";
import { useAgentCommandStore } from "./agentCommandStore";

// ---------------------------------------------------------------------------
// SSE event payloads (match ao-protocol AgentEventPayload workflow variants)
// ---------------------------------------------------------------------------

export interface WorkflowTaskCreatedEvent {
  task_id: string;
  workflow_id: string;
  project_name: string;
}

export interface PhaseStartedEvent {
  task_id: string;
  phase_id: string;
  phase_name: string;
}

export interface PhaseCompletedEvent {
  task_id: string;
  phase_id: string;
}

export interface PhaseSkippedEvent {
  task_id: string;
  phase_id: string;
  reason: string;
}

export interface PhaseFailedEvent {
  task_id: string;
  phase_id: string;
  error: string;
}

export interface PhasePausedEvent {
  task_id: string;
  phase_id: string;
  reason: string;
}

export interface WorkflowCompletedEvent {
  task_id: string;
}

export interface WorkflowTaskStartedEvent {
  task_id: string;
}

export interface WorkflowTaskFailedEvent {
  task_id: string;
  error: string;
}

export interface WorkflowTaskStoppedEvent {
  task_id: string;
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

interface WorkflowState {
  workflows: WorkflowSummary[];
  tasks: TaskSummary[];
  currentTask: TaskSnapshot | null;
  currentTaskId: string | null;
  loading: boolean;
  error: string | null;

  /** Cached output content keyed by "{task_id}/{filename}" */
  outputCache: Record<string, string>;

  archivedTasks: TaskSummary[];

  // Actions — API calls
  fetchWorkflows: () => Promise<void>;
  fetchTasks: () => Promise<void>;
  fetchArchivedTasks: () => Promise<void>;
  fetchTask: (id: string) => Promise<void>;
  createTask: (
    workflowId: string,
    projectName: string,
    workingDir?: string,
    context?: string,
  ) => Promise<string>;
  refreshWorkflows: () => Promise<void>;
  fetchOutput: (taskId: string, filename: string) => Promise<string>;

  // Actions — task management
  startTask: (taskId: string) => Promise<void>;
  resumeTask: (taskId: string) => Promise<void>;
  cancelTask: (taskId: string) => Promise<void>;
  deleteTask: (taskId: string) => Promise<void>;
  archiveTask: (taskId: string) => Promise<void>;

  // Actions — SSE event handlers
  handleTaskCreated: (e: WorkflowTaskCreatedEvent) => void;
  handlePhaseStarted: (e: PhaseStartedEvent) => void;
  handlePhaseCompleted: (e: PhaseCompletedEvent) => void;
  handlePhaseSkipped: (e: PhaseSkippedEvent) => void;
  handlePhaseFailed: (e: PhaseFailedEvent) => void;
  handlePhasePaused: (e: PhasePausedEvent) => void;
  handleWorkflowCompleted: (e: WorkflowCompletedEvent) => void;
  handleTaskStarted: (e: WorkflowTaskStartedEvent) => void;
  handleTaskFailed: (e: WorkflowTaskFailedEvent) => void;
  handleTaskStopped: (e: WorkflowTaskStoppedEvent) => void;
}

export const useWorkflowStore = create<WorkflowState>((set, get) => ({
  workflows: [],
  tasks: [],
  archivedTasks: [],
  currentTask: null,
  currentTaskId: null,
  loading: false,
  error: null,
  outputCache: {},

  fetchWorkflows: async () => {
    set({ loading: true, error: null });
    try {
      const workflows = await api.getWorkflows();
      set({ workflows });
    } catch (err) {
      set({ error: (err as Error).message });
    } finally {
      set({ loading: false });
    }
  },

  fetchTasks: async () => {
    set({ loading: true, error: null });
    try {
      const tasks = await api.getTasks();
      set({ tasks });
    } catch (err) {
      set({ error: (err as Error).message });
    } finally {
      set({ loading: false });
    }
  },

  fetchTask: async (id: string) => {
    // Clear stale task immediately so the UI shows a loading state
    // instead of rendering the previous task's data with mismatched phases.
    const prev = get().currentTaskId;
    if (prev !== id) {
      set({ currentTask: null, currentTaskId: id });
    }
    set({ loading: true, error: null });
    try {
      const snapshot = await api.getTask(id);
      const { currentTask, currentTaskId } = get();
      // Merge fetched snapshot with any optimistic phase updates already in state.
      // Prevents a stale fetch from overwriting a phase that was optimistically
      // advanced by an SSE event (e.g., PhaseStarted arriving before fetch resolves).
      if (currentTask && currentTaskId === id && currentTask.phases) {
        const STATUS_RANK: Record<string, number> = {
          pending: 0, running: 1, paused: 2, stopped: 3, completed: 4, skipped: 4, failed: 4,
        };
        const mergedPhases = { ...snapshot.phases };
        for (const [phaseId, localState] of Object.entries(currentTask.phases)) {
          const localRank = STATUS_RANK[(localState as { status: string }).status] ?? 0;
          const fetchedState = mergedPhases[phaseId] as { status: string } | undefined;
          const fetchedRank = fetchedState ? (STATUS_RANK[fetchedState.status] ?? 0) : 0;
          if (localRank > fetchedRank) {
            mergedPhases[phaseId] = localState;
          }
        }
        snapshot.phases = mergedPhases;
      }
      set({ currentTask: snapshot, currentTaskId: id });
    } catch (err) {
      set({ error: (err as Error).message });
    } finally {
      set({ loading: false });
    }
  },

  createTask: async (
    workflowId: string,
    projectName: string,
    workingDir?: string,
    context?: string,
  ) => {
    set({ loading: true, error: null });
    try {
      const resp = await api.createTask(workflowId, {
        project_name: projectName,
        working_directory: workingDir ?? null,
        context: context ?? null,
      });
      // Immediately fetch the new task snapshot so the panel can render
      const snapshot = await api.getTask(resp.task_id);
      set({ currentTask: snapshot, currentTaskId: resp.task_id });
      // Refresh task list
      get().fetchTasks();
      return resp.task_id;
    } catch (err) {
      set({ error: (err as Error).message });
      throw err;
    } finally {
      set({ loading: false });
    }
  },

  refreshWorkflows: async () => {
    try {
      await api.refreshWorkflows();
      await get().fetchWorkflows();
      // Also clear agent command cache so commands are re-discovered
      useAgentCommandStore.getState().clearCache();
    } catch (err) {
      set({ error: (err as Error).message });
    }
  },

  fetchOutput: async (taskId: string, filename: string) => {
    const cacheKey = `${taskId}/${filename}`;
    const cached = get().outputCache[cacheKey];
    if (cached !== undefined) return cached;
    const content = await api.getTaskOutput(taskId, filename);
    set({ outputCache: { ...get().outputCache, [cacheKey]: content } });
    return content;
  },

  startTask: async (taskId: string) => {
    try {
      await api.startTask(taskId);
      get().fetchTask(taskId);
      get().fetchTasks();
    } catch (err) {
      set({ error: (err as Error).message });
    }
  },

  resumeTask: async (taskId: string) => {
    try {
      await api.resumeTask(taskId);
      // Re-fetch to get updated state
      get().fetchTask(taskId);
      get().fetchTasks();
    } catch (err) {
      set({ error: (err as Error).message });
    }
  },

  cancelTask: async (taskId: string) => {
    try {
      await api.cancelTask(taskId);
      // Re-fetch to get updated state
      get().fetchTask(taskId);
      get().fetchTasks();
    } catch (err) {
      set({ error: (err as Error).message });
    }
  },

  deleteTask: async (taskId: string) => {
    try {
      await api.deleteTask(taskId);
      // Remove from local state (both active and archived lists)
      set({
        tasks: get().tasks.filter((t) => t.task_id !== taskId),
        archivedTasks: get().archivedTasks.filter((t) => t.task_id !== taskId),
      });
      if (get().currentTaskId === taskId) {
        set({ currentTask: null, currentTaskId: null });
      }
    } catch (err) {
      set({ error: (err as Error).message });
    }
  },

  archiveTask: async (taskId: string) => {
    try {
      await api.archiveTask(taskId);
      // Remove from active tasks list
      set({ tasks: get().tasks.filter((t) => t.task_id !== taskId) });
      if (get().currentTaskId === taskId) {
        set({ currentTask: null, currentTaskId: null });
      }
    } catch (err) {
      set({ error: (err as Error).message });
    }
  },

  fetchArchivedTasks: async () => {
    set({ loading: true, error: null });
    try {
      const archivedTasks = await api.getTasks({ archived: true });
      set({ archivedTasks });
    } catch (err) {
      set({ error: (err as Error).message });
    } finally {
      set({ loading: false });
    }
  },

  // ---------------------------------------------------------------------------
  // SSE event handlers — update store reactively when workflow events arrive
  // ---------------------------------------------------------------------------

  handleTaskCreated: (e: WorkflowTaskCreatedEvent) => {
    // Set the current task and fetch the full snapshot
    set({ currentTaskId: e.task_id });
    get().fetchTask(e.task_id);
    get().fetchTasks();
  },

  handlePhaseStarted: (e: PhaseStartedEvent) => {
    const { currentTask, currentTaskId } = get();
    if (!currentTask || currentTaskId !== e.task_id) {
      get().fetchTask(e.task_id);
      return;
    }
    // Optimistically update the phase to running
    set({
      currentTask: {
        ...currentTask,
        phases: {
          ...currentTask.phases,
          [e.phase_id]: {
            status: "running",
            started_at: new Date().toISOString(),
          },
        },
      },
    });
    get().fetchTasks();
  },

  handlePhaseCompleted: (e: PhaseCompletedEvent) => {
    const { currentTask, currentTaskId } = get();
    if (!currentTask || currentTaskId !== e.task_id) {
      get().fetchTask(e.task_id);
      return;
    }
    // Optimistic update for immediate UI feedback
    set({
      currentTask: {
        ...currentTask,
        phases: {
          ...currentTask.phases,
          [e.phase_id]: {
            ...currentTask.phases[e.phase_id],
            status: "completed",
            completed_at: new Date().toISOString(),
          },
        },
      },
    });
    // Re-fetch to get token usage and any other backend-computed fields
    get().fetchTask(e.task_id);
  },

  handlePhaseSkipped: (e: PhaseSkippedEvent) => {
    const { currentTask, currentTaskId } = get();
    if (!currentTask || currentTaskId !== e.task_id) {
      get().fetchTask(e.task_id);
      return;
    }
    set({
      currentTask: {
        ...currentTask,
        phases: {
          ...currentTask.phases,
          [e.phase_id]: {
            ...currentTask.phases[e.phase_id],
            status: "skipped",
            skipped_at: new Date().toISOString(),
            reason: e.reason,
          },
        },
      },
    });
  },

  handlePhaseFailed: (e: PhaseFailedEvent) => {
    const { currentTask, currentTaskId } = get();
    if (!currentTask || currentTaskId !== e.task_id) {
      get().fetchTask(e.task_id);
      return;
    }
    set({
      currentTask: {
        ...currentTask,
        phases: {
          ...currentTask.phases,
          [e.phase_id]: {
            ...currentTask.phases[e.phase_id],
            status: "failed",
            failed_at: new Date().toISOString(),
            error: e.error,
          },
        },
      },
    });
  },

  handlePhasePaused: (e: PhasePausedEvent) => {
    const { currentTask, currentTaskId } = get();
    if (!currentTask || currentTaskId !== e.task_id) {
      get().fetchTask(e.task_id);
      return;
    }
    set({
      currentTask: {
        ...currentTask,
        phases: {
          ...currentTask.phases,
          [e.phase_id]: {
            ...currentTask.phases[e.phase_id],
            status: "paused",
            paused_reason: e.reason,
          },
        },
      },
    });
  },

  handleWorkflowCompleted: (e: WorkflowCompletedEvent) => {
    const { currentTaskId } = get();
    if (currentTaskId === e.task_id) {
      // Re-fetch the final snapshot to get complete state
      get().fetchTask(e.task_id);
    }
    // Refresh task list to update status
    get().fetchTasks();
  },

  handleTaskStarted: (e: WorkflowTaskStartedEvent) => {
    // Optimistically update the matching task's status in the task list
    set({
      tasks: get().tasks.map((t) =>
        t.task_id === e.task_id ? { ...t, status: "running" as const } : t,
      ),
    });
    const { currentTask, currentTaskId } = get();
    if (currentTask && currentTaskId === e.task_id) {
      set({ currentTask: { ...currentTask, status: "running" } });
    }
    get().fetchTasks();
  },

  handleTaskFailed: (e: WorkflowTaskFailedEvent) => {
    // Optimistically update the matching task's status in the task list
    set({
      tasks: get().tasks.map((t) =>
        t.task_id === e.task_id ? { ...t, status: "failed" as const } : t,
      ),
    });
    const { currentTask, currentTaskId } = get();
    if (currentTask && currentTaskId === e.task_id) {
      set({ currentTask: { ...currentTask, status: "failed" } });
    }
    get().fetchTasks();
    // Refresh task list to get full state
    get().fetchTasks();
  },

  handleTaskStopped: (e: WorkflowTaskStoppedEvent) => {
    // Optimistically update the matching task's status in the task list
    set({
      tasks: get().tasks.map((t) =>
        t.task_id === e.task_id ? { ...t, status: "stopped" as const } : t,
      ),
    });
    const { currentTask, currentTaskId } = get();
    if (currentTask && currentTaskId === e.task_id) {
      // Set task status to stopped and mark the running phase as stopped
      const updatedPhases = { ...currentTask.phases };
      for (const [phaseId, phase] of Object.entries(updatedPhases)) {
        if (phase.status === "running") {
          updatedPhases[phaseId] = { ...phase, status: "stopped" };
        }
      }
      set({
        currentTask: {
          ...currentTask,
          status: "stopped",
          phases: updatedPhases,
        },
      });
    }
    get().fetchTasks();
  },
}));
