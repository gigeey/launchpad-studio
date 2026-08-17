import { create } from "zustand";
import * as api from "../lib/api";
import type { Task, Tasklist, TasklistStatus } from "../types/api";
import { noteTaskStatus } from "../lib/taskTimers";

/** Stamp/clear the live run-timer origin for every task in a tasklist. Called
 *  from the SSE + hydrate ingress so the elapsed indicator has an accurate
 *  origin regardless of which path delivered the status. */
function noteTasklistStatuses(tasklist: Tasklist | null): void {
  if (!tasklist) return;
  for (const g of tasklist.groups) {
    for (const t of g.tasks) {
      noteTaskStatus(tasklist.id, t.id, t.status);
    }
  }
}

interface AgentTasklistEntry {
  active: Tasklist | null;
  recent: Tasklist[];
  loading: boolean;
  error: string | null;
}

interface AgentTasklistState {
  byAgent: Map<string, AgentTasklistEntry>;

  hydrate: (agentId: string) => Promise<void>;
  applyTasklistCreated: (agentId: string, tasklistId: string) => Promise<void>;
  applyTaskUpdated: (
    agentId: string,
    tasklistId: string,
    task: Task,
  ) => Promise<void>;
  applyTaskAdded: (
    agentId: string,
    tasklistId: string,
    taskId: string,
  ) => Promise<void>;
  applyTasklistCompleted: (agentId: string, tasklistId: string) => void;
  applyTasklistFailed: (
    agentId: string,
    tasklistId: string,
    reason: string | null,
  ) => void;
  applyTasklistStatusChanged: (
    agentId: string,
    tasklistId: string,
    status: TasklistStatus,
  ) => void;
  reset: () => void;
}

const EMPTY_ENTRY: AgentTasklistEntry = {
  active: null,
  recent: [],
  loading: false,
  error: null,
};

function cloneByAgent(
  byAgent: Map<string, AgentTasklistEntry>,
): Map<string, AgentTasklistEntry> {
  return new Map(byAgent);
}

function getEntry(
  byAgent: Map<string, AgentTasklistEntry>,
  agentId: string,
): AgentTasklistEntry {
  return byAgent.get(agentId) ?? EMPTY_ENTRY;
}

function patchTaskInTasklist(
  tasklist: Tasklist,
  taskId: string,
  patch: Partial<Task>,
): Tasklist {
  let changed = false;
  const groups = tasklist.groups.map((g) => {
    let groupChanged = false;
    const tasks = g.tasks.map((t) => {
      if (t.id !== taskId) return t;
      groupChanged = true;
      return { ...t, ...patch };
    });
    if (!groupChanged) return g;
    changed = true;
    return { ...g, tasks };
  });
  if (!changed) return tasklist;
  return { ...tasklist, groups };
}

export const useAgentTasklistStore = create<AgentTasklistState>((set, get) => ({
  byAgent: new Map<string, AgentTasklistEntry>(),

  hydrate: async (agentId: string) => {
    const current = getEntry(get().byAgent, agentId);
    const next = cloneByAgent(get().byAgent);
    next.set(agentId, { ...current, loading: true, error: null });
    set({ byAgent: next });

    try {
      const resp = await api.listAgentTasklists(agentId);
      // Seed live run-timer origins from the hydrated snapshot so a reload
      // mid-run keeps an accurate "running since" for any in-progress task.
      noteTasklistStatuses(resp.active);
      const after = cloneByAgent(get().byAgent);
      after.set(agentId, {
        active: resp.active,
        recent: resp.recent,
        loading: false,
        error: null,
      });
      set({ byAgent: after });
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      const after = cloneByAgent(get().byAgent);
      const prev = getEntry(after, agentId);
      after.set(agentId, { ...prev, loading: false, error: message });
      set({ byAgent: after });
    }
  },

  applyTasklistCreated: async (agentId: string, tasklistId: string) => {
    try {
      const tl = await api.getAgentTasklist(agentId, tasklistId);
      // Belt-and-suspenders: if the fetched tasklist belongs to a project,
      // don't let it into the agent store (handles the race where the initial
      // TasklistCreated SSE fires before project_id is stamped server-side).
      if (tl.project_id) return;
      noteTasklistStatuses(tl);
      const next = cloneByAgent(get().byAgent);
      const prev = getEntry(next, agentId);
      const recent =
        prev.active && prev.active.id !== tl.id
          ? [prev.active, ...prev.recent]
          : prev.recent;
      next.set(agentId, {
        active: tl,
        recent,
        loading: prev.loading,
        error: null,
      });
      set({ byAgent: next });
    } catch (err) {
      console.warn(
        `[agentTasklistStore] failed to fetch created tasklist ${tasklistId}: ${err}`,
      );
    }
  },

  applyTaskUpdated: async (
    agentId: string,
    tasklistId: string,
    task: Task,
  ) => {
    const next = cloneByAgent(get().byAgent);
    const prev = getEntry(next, agentId);
    const taskId = task.id;
    const patch: Partial<Task> = task;

    // Capture the live run-timer origin the instant a task enters
    // `in_progress` (and clear it on any non-running transition).
    noteTaskStatus(tasklistId, taskId, task.status);

    const updateInList = (list: Tasklist[]): Tasklist[] =>
      list.map((tl) =>
        tl.id === tasklistId ? patchTaskInTasklist(tl, taskId, patch) : tl,
      );

    let active = prev.active;
    let recent = prev.recent;
    let touched = false;

    if (active && active.id === tasklistId) {
      const updated = patchTaskInTasklist(active, taskId, patch);
      if (updated !== active) {
        active = updated;
        touched = true;
      }
    } else if (recent.some((tl) => tl.id === tasklistId)) {
      const updated = updateInList(recent);
      if (updated !== recent) {
        recent = updated;
        touched = true;
      }
    } else {
      try {
        const tl = await api.getAgentTasklist(agentId, tasklistId);
        const after = cloneByAgent(get().byAgent);
        const cur = getEntry(after, agentId);
        const insertActive = tl.status === "active";
        after.set(agentId, {
          active: insertActive ? tl : cur.active,
          recent: insertActive
            ? cur.recent
            : [tl, ...cur.recent.filter((r) => r.id !== tl.id)],
          loading: cur.loading,
          error: null,
        });
        set({ byAgent: after });
        noteTasklistStatuses(tl);
      } catch (err) {
        console.warn(
          `[agentTasklistStore] failed to fetch tasklist ${tasklistId} for task update: ${err}`,
        );
      }
      return;
    }

    if (!touched) return;
    next.set(agentId, { ...prev, active, recent });
    set({ byAgent: next });
  },

  applyTaskAdded: async (
    agentId: string,
    tasklistId: string,
    _taskId: string,
  ) => {
    try {
      const tl = await api.getAgentTasklist(agentId, tasklistId);
      noteTasklistStatuses(tl);
      const next = cloneByAgent(get().byAgent);
      const prev = getEntry(next, agentId);

      if (prev.active && prev.active.id === tasklistId) {
        next.set(agentId, { ...prev, active: tl });
        set({ byAgent: next });
        return;
      }

      const idx = prev.recent.findIndex((t) => t.id === tasklistId);
      if (idx >= 0) {
        const recent = prev.recent.slice();
        recent[idx] = tl;
        next.set(agentId, { ...prev, recent });
        set({ byAgent: next });
        return;
      }

      const occupiesActiveSlot =
        tl.status === "active" || tl.status === "paused";
      next.set(agentId, {
        ...prev,
        active: occupiesActiveSlot ? tl : prev.active,
        recent: occupiesActiveSlot
          ? prev.recent
          : [tl, ...prev.recent.filter((r) => r.id !== tl.id)],
      });
      set({ byAgent: next });
    } catch (err) {
      console.warn(
        `[agentTasklistStore] failed to refetch tasklist ${tasklistId} for task added: ${err}`,
      );
    }
  },

  applyTasklistCompleted: (agentId: string, tasklistId: string) => {
    const current = get().byAgent.get(agentId);
    if (!current) return;
    const next = cloneByAgent(get().byAgent);
    const prev = current;
    if (prev.active && prev.active.id === tasklistId) {
      const completed: Tasklist = { ...prev.active, status: "completed" };
      next.set(agentId, {
        ...prev,
        active: null,
        recent: [completed, ...prev.recent],
      });
      set({ byAgent: next });
      return;
    }
    const idx = prev.recent.findIndex((tl) => tl.id === tasklistId);
    if (idx >= 0) {
      const recent = prev.recent.slice();
      recent[idx] = { ...recent[idx], status: "completed" };
      next.set(agentId, { ...prev, recent });
      set({ byAgent: next });
    }
  },

  applyTasklistFailed: (
    agentId: string,
    tasklistId: string,
    _reason: string | null,
  ) => {
    const current = get().byAgent.get(agentId);
    if (!current) return;
    const next = cloneByAgent(get().byAgent);
    const prev = current;
    if (prev.active && prev.active.id === tasklistId) {
      next.set(agentId, {
        ...prev,
        active: null,
        recent: [{ ...prev.active, status: "failed" }, ...prev.recent],
      });
      set({ byAgent: next });
      return;
    }
    const idx = prev.recent.findIndex((tl) => tl.id === tasklistId);
    if (idx >= 0) {
      const recent = prev.recent.slice();
      recent[idx] = { ...recent[idx], status: "failed" };
      next.set(agentId, { ...prev, recent });
      set({ byAgent: next });
    }
  },

  applyTasklistStatusChanged: (
    agentId: string,
    tasklistId: string,
    status: TasklistStatus,
  ) => {
    const current = get().byAgent.get(agentId);
    if (!current) return;
    const next = cloneByAgent(get().byAgent);
    const prev = current;
    if (prev.active && prev.active.id === tasklistId) {
      const isTerminal = status === "completed" || status === "failed" || status === "cancelled";
      if (isTerminal) {
        next.set(agentId, {
          ...prev,
          active: null,
          recent: [{ ...prev.active, status }, ...prev.recent],
        });
      } else {
        next.set(agentId, { ...prev, active: { ...prev.active, status } });
      }
      set({ byAgent: next });
      return;
    }
    const idx = prev.recent.findIndex((tl) => tl.id === tasklistId);
    if (idx >= 0) {
      const recent = prev.recent.slice();
      recent[idx] = { ...recent[idx], status };
      next.set(agentId, { ...prev, recent });
      set({ byAgent: next });
    }
  },

  reset: () => {
    set({ byAgent: new Map<string, AgentTasklistEntry>() });
  },
}));

export function useAgentTasklistsForAgent(
  agentId: string | null,
): AgentTasklistEntry {
  return useAgentTasklistStore((s) =>
    agentId ? s.byAgent.get(agentId) ?? EMPTY_ENTRY : EMPTY_ENTRY,
  );
}

/** Count in-progress tasks across all groups of a tasklist. */
export function countInProgress(tasklist: Tasklist | null): number {
  if (!tasklist) return 0;
  let count = 0;
  for (const g of tasklist.groups) {
    for (const t of g.tasks) {
      if (t.status === "in_progress") count++;
    }
  }
  return count;
}
