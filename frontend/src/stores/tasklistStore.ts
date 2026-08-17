import { useEffect, useMemo, useCallback } from "react";
import { create } from "zustand";
import * as api from "../lib/api";
import type {
  AgentSnapshot,
  Task,
  TaskDetail,
  TaskDetailAssignedAgent,
  Tasklist,
  TasklistScope,
  TasklistStatus,
} from "../types/api";
import { scopeKey } from "../types/api";
import { useChatStore } from "./chatStore";

export type { TasklistScope };
export { scopeKey };

interface ScopeTasklistEntry {
  active: Tasklist | null;
  recent: Tasklist[];
  loading: boolean;
  error: string | null;
}

interface TaskDetailFetchState {
  loading: boolean;
  error: string | null;
}

interface TasklistState {
  byScope: Map<string, ScopeTasklistEntry>;
  selectedByScope: Map<string, string>;
  taskDetailFetchByTaskId: Map<string, TaskDetailFetchState>;

  hydrate: (scope: TasklistScope) => Promise<void>;
  fetchTaskDetail: (scope: TasklistScope, taskId: string) => Promise<TaskDetail>;
  applyTasklistCreated: (scope: TasklistScope, tasklistId: string) => Promise<void>;
  applyTaskUpdated: (scope: TasklistScope, tasklistId: string, task: Task) => Promise<void>;
  applyTaskAdded: (scope: TasklistScope, tasklistId: string, taskId: string) => Promise<void>;
  refreshTasklist: (scope: TasklistScope, tasklistId: string) => Promise<void>;
  applyTasklistCompleted: (scope: TasklistScope, tasklistId: string) => void;
  applyTasklistFailed: (scope: TasklistScope, tasklistId: string, reason: string | null) => void;
  applyTasklistStatusChanged: (scope: TasklistScope, tasklistId: string, status: TasklistStatus) => void;
  setTasklistStatus: (scope: TasklistScope, tasklistId: string, status: "active" | "paused") => Promise<void>;
  continueTasklist: (scope: TasklistScope, tasklistId: string) => Promise<void>;
  skipTask: (scope: TasklistScope, tasklistId: string, taskId: string) => Promise<void>;
  stopTask: (scope: TasklistScope, tasklistId: string, taskId: string) => Promise<void>;
  resumeTask: (scope: TasklistScope, tasklistId: string, taskId: string) => Promise<void>;
  discardTasklist: (scope: TasklistScope, tasklistId: string) => Promise<void>;
  replayTasklist: (scope: TasklistScope, tasklistId: string) => Promise<void>;
  setSelectedTasklist: (scope: TasklistScope, tasklistId: string | null) => void;
  reset: () => void;
}

const EMPTY_ENTRY: ScopeTasklistEntry = {
  active: null,
  recent: [],
  loading: false,
  error: null,
};

function cloneByScope(m: Map<string, ScopeTasklistEntry>): Map<string, ScopeTasklistEntry> {
  return new Map(m);
}

function getEntry(m: Map<string, ScopeTasklistEntry>, key: string): ScopeTasklistEntry {
  return m.get(key) ?? EMPTY_ENTRY;
}

function findTaskInScope(
  byScope: Map<string, ScopeTasklistEntry>,
  scope: TasklistScope,
  taskId: string,
): { task: Task; tasklist: Tasklist } | null {
  const entry = byScope.get(scopeKey(scope));
  if (!entry) return null;
  const lists = entry.active ? [entry.active, ...entry.recent] : entry.recent;
  for (const tl of lists) {
    for (const g of tl.groups) {
      for (const t of g.tasks) {
        if (t.id === taskId) return { task: t, tasklist: tl };
      }
    }
  }
  return null;
}

function buildTaskDetail(task: Task, agents: AgentSnapshot[]): TaskDetail {
  const title = task.prompt.split("\n")[0]?.trim() || task.prompt;
  let assigned_agent: TaskDetailAssignedAgent | null = null;
  // assignment.owner_agent_id wins (coordinator-routed tasks); legacy field is fallback
  const ownerId = task.assignment?.owner_agent_id || task.owner_agent_id || "";
  if (ownerId) {
    const a = agents.find((s) => s.agent_id === ownerId);
    assigned_agent = a
      ? { id: a.agent_id, name: a.name, emoji: a.emoji }
      : { id: ownerId, name: "Agent", emoji: undefined };
  }
  return { ...task, title, assigned_agent };
}

function patchTaskInTasklist(tasklist: Tasklist, taskId: string, patch: Partial<Task>): Tasklist {
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

export const useTasklistStore = create<TasklistState>((set, get) => ({
  byScope: new Map<string, ScopeTasklistEntry>(),
  selectedByScope: new Map<string, string>(),
  taskDetailFetchByTaskId: new Map<string, TaskDetailFetchState>(),

  hydrate: async (scope: TasklistScope) => {
    const key = scopeKey(scope);
    const current = getEntry(get().byScope, key);
    const next = cloneByScope(get().byScope);
    next.set(key, { ...current, loading: true, error: null });
    set({ byScope: next });

    try {
      const resp = await api.listTasklistsForScope(scope);
      const after = cloneByScope(get().byScope);
      after.set(key, { active: resp.active, recent: resp.recent, loading: false, error: null });
      set({ byScope: after });
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      const after = cloneByScope(get().byScope);
      const prev = getEntry(after, key);
      after.set(key, { ...prev, loading: false, error: message });
      set({ byScope: after });
    }
  },

  fetchTaskDetail: async (scope: TasklistScope, taskId: string): Promise<TaskDetail> => {
    const setFetchState = (state: TaskDetailFetchState) => {
      const next = new Map(get().taskDetailFetchByTaskId);
      next.set(taskId, state);
      set({ taskDetailFetchByTaskId: next });
    };

    setFetchState({ loading: true, error: null });

    try {
      let hit = findTaskInScope(get().byScope, scope, taskId);
      if (!hit) {
        await get().hydrate(scope);
        hit = findTaskInScope(get().byScope, scope, taskId);
      }
      if (!hit) {
        throw new Error(`Task ${taskId} not found in scope ${scopeKey(scope)}`);
      }
      const agents = useChatStore.getState().agents;
      const detail = buildTaskDetail(hit.task, agents);
      setFetchState({ loading: false, error: null });
      return detail;
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      setFetchState({ loading: false, error: message });
      throw err;
    }
  },

  applyTasklistCreated: async (scope: TasklistScope, tasklistId: string) => {
    try {
      const tl = await api.getTasklistForScope(scope, tasklistId);
      const key = scopeKey(scope);
      const next = cloneByScope(get().byScope);
      const prev = getEntry(next, key);
      const recent =
        prev.active && prev.active.id !== tl.id
          ? [prev.active, ...prev.recent]
          : prev.recent;
      next.set(key, { active: tl, recent, loading: prev.loading, error: null });
      const nextSelected = new Map(get().selectedByScope);
      nextSelected.delete(key);
      set({ byScope: next, selectedByScope: nextSelected });
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      console.warn(`[tasklistStore] failed to fetch created tasklist ${tasklistId}: ${message}`);
    }
  },

  applyTaskUpdated: async (scope: TasklistScope, tasklistId: string, task: Task) => {
    const key = scopeKey(scope);
    const next = cloneByScope(get().byScope);
    const prev = getEntry(next, key);
    const taskId = task.id;
    const patch: Partial<Task> = task;

    const updateInList = (list: Tasklist[]): Tasklist[] =>
      list.map((tl) => (tl.id === tasklistId ? patchTaskInTasklist(tl, taskId, patch) : tl));

    let active = prev.active;
    let recent = prev.recent;
    let touched = false;
    if (active && active.id === tasklistId) {
      const updated = patchTaskInTasklist(active, taskId, patch);
      if (updated !== active) { active = updated; touched = true; }
    } else if (recent.some((tl) => tl.id === tasklistId)) {
      const updated = updateInList(recent);
      if (updated !== recent) { recent = updated; touched = true; }
    } else {
      try {
        const tl = await api.getTasklistForScope(scope, tasklistId);
        const after = cloneByScope(get().byScope);
        const cur = getEntry(after, key);
        const insertActive = tl.status === "active";
        after.set(key, {
          active: insertActive ? tl : cur.active,
          recent: insertActive ? cur.recent : [tl, ...cur.recent.filter((r) => r.id !== tl.id)],
          loading: cur.loading,
          error: null,
        });
        set({ byScope: after });
      } catch (err) {
        console.warn(`[tasklistStore] failed to fetch tasklist ${tasklistId} for task update: ${err}`);
      }
      return;
    }

    if (!touched) return;
    next.set(key, { ...prev, active, recent });
    set({ byScope: next });
  },

  applyTaskAdded: async (scope: TasklistScope, tasklistId: string, _taskId: string) => {
    const key = scopeKey(scope);
    try {
      const tl = await api.getTasklistForScope(scope, tasklistId);
      const next = cloneByScope(get().byScope);
      const prev = getEntry(next, key);
      if (prev.active && prev.active.id === tasklistId) {
        next.set(key, { ...prev, active: tl });
        set({ byScope: next });
        return;
      }
      const idx = prev.recent.findIndex((t) => t.id === tasklistId);
      if (idx >= 0) {
        const recent = prev.recent.slice();
        recent[idx] = tl;
        next.set(key, { ...prev, recent });
        set({ byScope: next });
        return;
      }
      const occupiesActiveSlot = tl.status === "active" || tl.status === "paused";
      next.set(key, {
        ...prev,
        active: occupiesActiveSlot ? tl : prev.active,
        recent: occupiesActiveSlot ? prev.recent : [tl, ...prev.recent.filter((r) => r.id !== tl.id)],
      });
      set({ byScope: next });
    } catch (err) {
      console.warn(`[tasklistStore] failed to refetch tasklist ${tasklistId} for task added: ${err}`);
    }
  },

  refreshTasklist: async (scope: TasklistScope, tasklistId: string) => {
    const key = scopeKey(scope);
    try {
      const tl = await api.getTasklistForScope(scope, tasklistId);
      const next = cloneByScope(get().byScope);
      const prev = getEntry(next, key);
      if (prev.active && prev.active.id === tasklistId) {
        next.set(key, { ...prev, active: tl });
        set({ byScope: next });
        return;
      }
      const idx = prev.recent.findIndex((t) => t.id === tasklistId);
      if (idx >= 0) {
        const recent = prev.recent.slice();
        recent[idx] = tl;
        next.set(key, { ...prev, recent });
        set({ byScope: next });
      }
    } catch (err) {
      console.warn(`[tasklistStore] failed to refresh tasklist ${tasklistId}: ${err}`);
    }
  },

  applyTasklistCompleted: (scope: TasklistScope, tasklistId: string) => {
    const key = scopeKey(scope);
    const next = cloneByScope(get().byScope);
    const prev = getEntry(next, key);
    if (prev.active && prev.active.id === tasklistId) {
      const completed: Tasklist = { ...prev.active, status: "completed" };
      next.set(key, { ...prev, active: null, recent: [completed, ...prev.recent] });
      set({ byScope: next });
      return;
    }
    const recent = prev.recent.map((tl) =>
      tl.id === tasklistId ? { ...tl, status: "completed" as const } : tl,
    );
    if (recent !== prev.recent) {
      next.set(key, { ...prev, recent });
      set({ byScope: next });
    }
  },

  applyTasklistFailed: (scope: TasklistScope, tasklistId: string, _reason: string | null) => {
    const key = scopeKey(scope);
    const next = cloneByScope(get().byScope);
    const prev = getEntry(next, key);
    const fail = (tl: Tasklist): Tasklist => ({ ...tl, status: "failed" });
    if (prev.active && prev.active.id === tasklistId) {
      next.set(key, { ...prev, active: null, recent: [fail(prev.active), ...prev.recent] });
      set({ byScope: next });
      return;
    }
    const recent = prev.recent.map((tl) => (tl.id === tasklistId ? fail(tl) : tl));
    if (recent !== prev.recent) {
      next.set(key, { ...prev, recent });
      set({ byScope: next });
    }
  },

  applyTasklistStatusChanged: (scope: TasklistScope, tasklistId: string, status: TasklistStatus) => {
    const key = scopeKey(scope);
    const next = cloneByScope(get().byScope);
    const prev = getEntry(next, key);
    if (prev.active && prev.active.id === tasklistId) {
      next.set(key, { ...prev, active: { ...prev.active, status } });
      set({ byScope: next });
      return;
    }
    const recent = prev.recent.map((tl) => (tl.id === tasklistId ? { ...tl, status } : tl));
    if (recent !== prev.recent) {
      next.set(key, { ...prev, recent });
      set({ byScope: next });
    }
  },

  setTasklistStatus: async (scope: TasklistScope, tasklistId: string, status: "active" | "paused") => {
    get().applyTasklistStatusChanged(scope, tasklistId, status);
    try {
      await api.setTasklistStatusForScope(scope, tasklistId, status);
    } catch (err) {
      try {
        const tl = await api.getTasklistForScope(scope, tasklistId);
        get().applyTasklistStatusChanged(scope, tasklistId, tl.status);
      } catch {
        /* swallow refetch error */
      }
      throw err;
    }
  },

  continueTasklist: async (scope: TasklistScope, tasklistId: string) => {
    const key = scopeKey(scope);
    const updated = await api.continueTasklistForScope(scope, tasklistId);
    const next = cloneByScope(get().byScope);
    const prev = getEntry(next, key);
    const recent = (
      prev.active && prev.active.id !== updated.id
        ? [prev.active, ...prev.recent]
        : prev.recent
    ).filter((tl) => tl.id !== updated.id);
    next.set(key, { active: updated, recent, loading: prev.loading, error: null });
    const nextSelected = new Map(get().selectedByScope);
    nextSelected.delete(key);
    set({ byScope: next, selectedByScope: nextSelected });
  },

  skipTask: async (scope: TasklistScope, tasklistId: string, taskId: string) => {
    const key = scopeKey(scope);
    const updated = await api.skipTaskForScope(scope, tasklistId, taskId);
    const next = cloneByScope(get().byScope);
    const prev = getEntry(next, key);
    if (updated.status === "active") {
      const recent = (
        prev.active && prev.active.id !== updated.id
          ? [prev.active, ...prev.recent]
          : prev.recent
      ).filter((tl) => tl.id !== updated.id);
      next.set(key, { active: updated, recent, loading: prev.loading, error: null });
      const nextSelected = new Map(get().selectedByScope);
      nextSelected.delete(key);
      set({ byScope: next, selectedByScope: nextSelected });
      return;
    }
    const active = prev.active && prev.active.id === updated.id ? updated : prev.active;
    const recent = prev.recent.map((tl) => (tl.id === updated.id ? updated : tl));
    next.set(key, { active, recent, loading: prev.loading, error: prev.error });
    set({ byScope: next });
  },

  stopTask: async (scope: TasklistScope, tasklistId: string, taskId: string) => {
    const key = scopeKey(scope);
    const updated = await api.stopTaskForScope(scope, tasklistId, taskId);
    const next = cloneByScope(get().byScope);
    const prev = getEntry(next, key);
    const active = prev.active && prev.active.id === updated.id ? updated : prev.active;
    const recent = prev.recent.map((tl) => (tl.id === updated.id ? updated : tl));
    next.set(key, { active, recent, loading: prev.loading, error: prev.error });
    set({ byScope: next });
  },

  resumeTask: async (scope: TasklistScope, tasklistId: string, taskId: string) => {
    const key = scopeKey(scope);
    const updated = await api.resumeTaskForScope(scope, tasklistId, taskId);
    const next = cloneByScope(get().byScope);
    const prev = getEntry(next, key);
    const active = prev.active && prev.active.id === updated.id ? updated : prev.active;
    const recent = prev.recent.map((tl) => (tl.id === updated.id ? updated : tl));
    next.set(key, { active, recent, loading: prev.loading, error: prev.error });
    set({ byScope: next });
  },

  discardTasklist: async (scope: TasklistScope, tasklistId: string) => {
    const key = scopeKey(scope);
    const updated = await api.discardTasklistForScope(scope, tasklistId);
    const next = cloneByScope(get().byScope);
    const prev = getEntry(next, key);
    const wasActive = prev.active && prev.active.id === updated.id;
    const recent = wasActive
      ? [updated, ...prev.recent]
      : prev.recent.map((tl) => (tl.id === updated.id ? updated : tl));
    next.set(key, { active: wasActive ? null : prev.active, recent, loading: prev.loading, error: null });
    set({ byScope: next });
  },

  replayTasklist: async (scope: TasklistScope, tasklistId: string) => {
    const key = scopeKey(scope);
    const created = await api.replayTasklistForScope(scope, tasklistId);
    const next = cloneByScope(get().byScope);
    const prev = getEntry(next, key);
    const recent =
      prev.active && prev.active.id !== created.id
        ? [prev.active, ...prev.recent]
        : prev.recent;
    next.set(key, { active: created, recent, loading: prev.loading, error: null });
    const nextSelected = new Map(get().selectedByScope);
    nextSelected.delete(key);
    set({ byScope: next, selectedByScope: nextSelected });
  },

  setSelectedTasklist: (scope: TasklistScope, tasklistId: string | null) => {
    const key = scopeKey(scope);
    const next = new Map(get().selectedByScope);
    if (tasklistId === null) {
      next.delete(key);
    } else {
      next.set(key, tasklistId);
    }
    set({ selectedByScope: next });
  },

  reset: () => {
    set({
      byScope: new Map<string, ScopeTasklistEntry>(),
      selectedByScope: new Map<string, string>(),
      taskDetailFetchByTaskId: new Map<string, TaskDetailFetchState>(),
    });
  },
}));

// ---------------------------------------------------------------------------
// Scope-aware selectors
// ---------------------------------------------------------------------------

export function useTasklistsForScope(scope: TasklistScope | null): ScopeTasklistEntry {
  return useTasklistStore((s) =>
    scope ? s.byScope.get(scopeKey(scope)) ?? EMPTY_ENTRY : EMPTY_ENTRY,
  );
}

interface CurrentAndArchivedEntry {
  current: Tasklist | null;
  archived: Tasklist[];
  all: Tasklist[];
  selectedId: string | null;
  loading: boolean;
  error: string | null;
}

export function useCurrentAndArchivedTasklistsForScope(
  scope: TasklistScope | null,
): CurrentAndArchivedEntry {
  const entry = useTasklistsForScope(scope);
  const selectedId = useTasklistStore((s) =>
    scope ? s.selectedByScope.get(scopeKey(scope)) ?? null : null,
  );

  // Memoize derived arrays so downstream effects using these as deps don't
  // fire on every parent re-render when the store data hasn't changed.
  return useMemo(() => {
    const all: Tasklist[] = entry.active ? [entry.active, ...entry.recent] : entry.recent;

    let current: Tasklist | null = null;
    if (selectedId) {
      current = all.find((tl) => tl.id === selectedId) ?? null;
    }
    if (!current) {
      current = entry.active ?? entry.recent[0] ?? null;
    }
    const archived = current ? all.filter((tl) => tl.id !== current!.id) : all;

    return { current, archived, all, selectedId, loading: entry.loading, error: entry.error };
  }, [entry, selectedId]);
}

// ---------------------------------------------------------------------------
// useTaskDetail hook
// ---------------------------------------------------------------------------

interface UseTaskDetailResult {
  data: TaskDetail | null;
  tasklistId: string | null;
  loading: boolean;
  error: string | null;
  refetch: () => void;
}

export function useTaskDetail(
  scope: TasklistScope | null,
  taskId: string | null,
): UseTaskDetailResult {
  const fetchTaskDetail = useTasklistStore((s) => s.fetchTaskDetail);
  const byScope = useTasklistStore((s) => s.byScope);
  const agents = useChatStore((s) => s.agents);
  const fetchState = useTasklistStore((s) =>
    taskId ? s.taskDetailFetchByTaskId.get(taskId) ?? null : null,
  );

  const resolved = useMemo<{ data: TaskDetail | null; tasklistId: string | null }>(() => {
    if (!scope || !taskId) return { data: null, tasklistId: null };
    const hit = findTaskInScope(byScope, scope, taskId);
    if (!hit) return { data: null, tasklistId: null };
    return {
      data: buildTaskDetail(hit.task, agents),
      tasklistId: hit.tasklist.id,
    };
  }, [byScope, agents, scope, taskId]);

  useEffect(() => {
    if (!scope || !taskId) return;
    fetchTaskDetail(scope, taskId).catch(() => {});
  }, [scope, taskId, fetchTaskDetail]);

  const refetch = useCallback(() => {
    if (!scope || !taskId) return;
    fetchTaskDetail(scope, taskId).catch(() => {});
  }, [scope, taskId, fetchTaskDetail]);

  return {
    data: resolved.data,
    tasklistId: resolved.tasklistId,
    loading: fetchState?.loading ?? false,
    error: fetchState?.error ?? null,
    refetch,
  };
}
