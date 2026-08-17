import { create } from "zustand";
import * as api from "../lib/api";
import type { WorkflowSummary } from "../types/workflow";
import type { AgentProfile } from "../types/api";

type AgentWorkflows = AgentProfile["workflows"];

interface WorkflowsState {
  workflows: WorkflowSummary[];
  loading: boolean;
  refreshing: boolean;
  error: string | null;

  loadWorkflows: () => Promise<void>;
  refreshWorkflows: () => Promise<void>;
  importFolder: (sourcePath: string) => Promise<void>;
  setWorkflowEnabled: (
    agentId: string,
    workflowId: string,
    enabled: boolean,
  ) => Promise<AgentProfile>;
  setAllWorkflows: (agentId: string, enabled: boolean) => Promise<AgentProfile>;
  reset: () => void;
}

/** True if `agentWorkflows` grants access to the given workflow id.
 *  'all' → every id; array → membership; null/undefined → none. */
export function isWorkflowEnabled(
  agentWorkflows: AgentWorkflows,
  workflowId: string,
): boolean {
  if (agentWorkflows === "all") return true;
  if (Array.isArray(agentWorkflows)) return agentWorkflows.includes(workflowId);
  return false;
}

/** Resolve the set of enabled workflow ids, dropping orphans that no longer
 *  exist in `catalog`. Mirrors `AgentProfileModal`'s behavior of ignoring ids not
 *  in the available list. */
export function resolveEnabledWorkflowIds(
  agentWorkflows: AgentWorkflows,
  catalog: WorkflowSummary[],
): string[] {
  const catalogIds = catalog.map((w) => w.id);
  if (agentWorkflows === "all") return catalogIds;
  if (Array.isArray(agentWorkflows)) {
    const known = new Set(catalogIds);
    return agentWorkflows.filter((id) => known.has(id));
  }
  return [];
}

function errorMessage(err: unknown): string {
  if (err instanceof Error) return err.message;
  return String(err);
}

// Per-agent write queue: chaining promises serializes concurrent toggles so
// the read-modify-write cycle always sees the latest persisted profile.
const writeQueues = new Map<string, Promise<unknown>>();

function enqueueAgentWrite<T>(agentId: string, task: () => Promise<T>): Promise<T> {
  const previous = writeQueues.get(agentId) ?? Promise.resolve();
  const next = previous.catch(() => undefined).then(task);
  writeQueues.set(agentId, next);
  void next.finally(() => {
    if (writeQueues.get(agentId) === next) writeQueues.delete(agentId);
  });
  return next;
}

export const useWorkflowsStore = create<WorkflowsState>((set, get) => ({
  workflows: [],
  loading: false,
  refreshing: false,
  error: null,

  loadWorkflows: async () => {
    set({ loading: true, error: null });
    try {
      const workflows = await api.getWorkflows();
      set({ workflows, loading: false });
    } catch (err) {
      set({ loading: false, error: errorMessage(err) });
    }
  },

  refreshWorkflows: async () => {
    set({ refreshing: true, error: null });
    try {
      await api.refreshWorkflows();
      const workflows = await api.getWorkflows();
      set({ workflows, refreshing: false });
    } catch (err) {
      set({ refreshing: false, error: errorMessage(err) });
    }
  },

  importFolder: async (sourcePath: string) => {
    set({ error: null });
    try {
      await api.importWorkflow(sourcePath);
      const workflows = await api.getWorkflows();
      set({ workflows });
    } catch (err) {
      set({ error: errorMessage(err) });
      throw err;
    }
  },

  setWorkflowEnabled: (agentId, workflowId, enabled) =>
    enqueueAgentWrite(agentId, async () => {
      const profile = await api.getAgent(agentId);
      const catalog = get().workflows;
      const current = profile.workflows;
      let nextList: string[];
      if (current === "all") {
        nextList = catalog.map((w) => w.id);
      } else if (Array.isArray(current)) {
        nextList = [...current];
      } else {
        nextList = [];
      }
      if (enabled) {
        if (!nextList.includes(workflowId)) nextList.push(workflowId);
      } else {
        nextList = nextList.filter((id) => id !== workflowId);
      }
      return api.updateAgent({ ...profile, workflows: nextList });
    }),

  setAllWorkflows: (agentId, enabled) =>
    enqueueAgentWrite(agentId, async () => {
      const profile = await api.getAgent(agentId);
      const next: AgentWorkflows = enabled ? "all" : [];
      return api.updateAgent({ ...profile, workflows: next });
    }),

  reset: () => {
    set({ workflows: [], loading: false, refreshing: false, error: null });
  },
}));
