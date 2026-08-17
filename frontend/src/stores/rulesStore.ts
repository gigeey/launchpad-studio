import { create } from "zustand";
import * as api from "../lib/api";
import type { Rule } from "../lib/api";

interface RulesState {
  agentId: string | null;
  rules: Rule[];
  loading: boolean;
  refreshing: boolean;
  error: string | null;

  load: (agentId: string) => Promise<void>;
  refresh: () => Promise<void>;
  importFile: (path: string) => Promise<void>;
  importFolder: (path: string) => Promise<void>;
  importLink: (url: string) => Promise<void>;
  remove: (ruleId: string) => Promise<void>;
  setEnabled: (ruleId: string, enabled: boolean) => Promise<void>;
  setAllEnabled: (enabled: boolean) => Promise<void>;
  setAutoSync: (ruleId: string, autoSync: boolean) => Promise<void>;
  mergeRules: (incoming: Rule[]) => void;
  reset: () => void;
}

function errorMessage(err: unknown): string {
  if (err instanceof Error) return err.message;
  return String(err);
}

/** Folds newly-imported rules into an existing list. Replace-by-id so
 *  re-imports/re-scans overwrite stale entries; otherwise append. */
function mergeRules(existing: Rule[], incoming: Rule[]): Rule[] {
  if (incoming.length === 0) return existing;
  const byId = new Map(incoming.map((r) => [r.id, r]));
  const replaced = existing.map((r) => byId.get(r.id) ?? r);
  const seen = new Set(existing.map((r) => r.id));
  const appended = incoming.filter((r) => !seen.has(r.id));
  return [...replaced, ...appended];
}

export const useRulesStore = create<RulesState>((set, get) => ({
  agentId: null,
  rules: [],
  loading: false,
  refreshing: false,
  error: null,

  load: async (agentId: string) => {
    set({ agentId, loading: true, error: null });
    try {
      const rules = await api.listRules(agentId);
      if (get().agentId !== agentId) return;
      set({ rules, loading: false });
    } catch (err) {
      if (get().agentId !== agentId) return;
      set({ loading: false, error: errorMessage(err) });
    }
  },

  refresh: async () => {
    const { agentId } = get();
    if (!agentId) return;
    set({ refreshing: true, error: null });
    try {
      const rules = await api.refreshAgentRules(agentId);
      if (get().agentId !== agentId) return;
      set({ rules, refreshing: false });
    } catch (err) {
      if (get().agentId !== agentId) return;
      set({ refreshing: false, error: errorMessage(err) });
    }
  },

  importFile: async (path: string) => {
    const { agentId } = get();
    if (!agentId) return;
    set({ error: null });
    try {
      const imported = await api.importAgentRuleFile(agentId, path);
      if (get().agentId !== agentId) return;
      set((state) => ({ rules: mergeRules(state.rules, imported) }));
    } catch (err) {
      set({ error: errorMessage(err) });
      throw err;
    }
  },

  importFolder: async (path: string) => {
    const { agentId } = get();
    if (!agentId) return;
    set({ error: null });
    try {
      const imported = await api.importAgentRuleFolder(agentId, path);
      if (get().agentId !== agentId) return;
      set((state) => ({ rules: mergeRules(state.rules, imported) }));
    } catch (err) {
      set({ error: errorMessage(err) });
      throw err;
    }
  },

  importLink: async (url: string) => {
    const { agentId } = get();
    if (!agentId) return;
    set({ error: null });
    try {
      const imported = await api.importAgentRuleLink(agentId, url);
      if (get().agentId !== agentId) return;
      set((state) => ({ rules: mergeRules(state.rules, imported) }));
    } catch (err) {
      set({ error: errorMessage(err) });
      throw err;
    }
  },

  remove: async (ruleId: string) => {
    const { agentId, rules } = get();
    if (!agentId) return;
    const previous = rules;
    // Top-level deletes cascade — optimistically drop every rule whose id
    // starts with `<ruleId>/` (nested under the bundle) plus the bundle itself.
    const prefix = `${ruleId}/`;
    set({
      rules: rules.filter((r) => r.id !== ruleId && !r.id.startsWith(prefix)),
      error: null,
    });
    try {
      await api.deleteAgentRule(agentId, ruleId);
    } catch (err) {
      if (get().agentId !== agentId) return;
      set({ rules: previous, error: errorMessage(err) });
      throw err;
    }
  },

  setEnabled: async (ruleId: string, enabled: boolean) => {
    const { agentId, rules } = get();
    if (!agentId) return;
    const previous = rules;
    set({
      rules: rules.map((r) => (r.id === ruleId ? { ...r, enabled } : r)),
      error: null,
    });
    try {
      const updated = await api.patchAgentRule(agentId, ruleId, { enabled });
      if (get().agentId !== agentId) return;
      set((state) => ({
        rules: state.rules.map((r) => (r.id === ruleId ? updated : r)),
      }));
    } catch (err) {
      if (get().agentId !== agentId) return;
      set({ rules: previous, error: errorMessage(err) });
    }
  },

  setAllEnabled: async (enabled: boolean) => {
    const { agentId, rules } = get();
    if (!agentId || rules.length === 0) return;
    const previous = rules;
    set({ rules: rules.map((r) => ({ ...r, enabled })), error: null });
    try {
      await Promise.all(
        rules.map((r) => api.patchAgentRule(agentId, r.id, { enabled })),
      );
    } catch (err) {
      if (get().agentId !== agentId) return;
      try {
        const refreshed = await api.listRules(agentId);
        if (get().agentId !== agentId) return;
        set({ rules: refreshed, error: errorMessage(err) });
      } catch {
        set({ rules: previous, error: errorMessage(err) });
      }
      throw err;
    }
  },

  setAutoSync: async (ruleId: string, autoSync: boolean) => {
    const { agentId, rules } = get();
    if (!agentId) return;
    const previous = rules;
    set({
      rules: rules.map((r) => (r.id === ruleId ? { ...r, auto_sync: autoSync } : r)),
      error: null,
    });
    try {
      const updated = await api.patchAgentRule(agentId, ruleId, { autoSync });
      if (get().agentId !== agentId) return;
      set((state) => ({
        rules: state.rules.map((r) => (r.id === ruleId ? updated : r)),
      }));
    } catch (err) {
      if (get().agentId !== agentId) return;
      set({ rules: previous, error: errorMessage(err) });
    }
  },

  mergeRules: (incoming: Rule[]) => {
    set((state) => ({ rules: mergeRules(state.rules, incoming) }));
  },

  reset: () => {
    set({ agentId: null, rules: [], loading: false, refreshing: false, error: null });
  },
}));
