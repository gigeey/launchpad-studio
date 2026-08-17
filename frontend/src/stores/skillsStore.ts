import { create } from "zustand";
import * as api from "../lib/api";
import type { Skill } from "../lib/api";

interface SkillsState {
  agentId: string | null;
  skills: Skill[];
  loading: boolean;
  refreshing: boolean;
  error: string | null;

  load: (agentId: string) => Promise<void>;
  refresh: () => Promise<void>;
  importFolder: (path: string) => Promise<void>;
  importFile: (path: string) => Promise<void>;
  remove: (skillId: string) => Promise<void>;
  setEnabled: (skillId: string, enabled: boolean) => Promise<void>;
  setAllEnabled: (enabled: boolean) => Promise<void>;
  setAutoSync: (skillId: string, autoSync: boolean) => Promise<void>;
  reset: () => void;
}

function errorMessage(err: unknown): string {
  if (err instanceof Error) return err.message;
  return String(err);
}

/** Folds newly-imported skills into an existing list. An incoming skill that
 *  shares an id with an existing one replaces it (covers re-imports); otherwise
 *  it's appended. Preserves ordering of the existing list. */
function mergeSkills(existing: Skill[], incoming: Skill[]): Skill[] {
  if (incoming.length === 0) return existing;
  const byId = new Map(incoming.map((s) => [s.id, s]));
  const replaced = existing.map((s) => byId.get(s.id) ?? s);
  const seen = new Set(existing.map((s) => s.id));
  const appended = incoming.filter((s) => !seen.has(s.id));
  return [...replaced, ...appended];
}

export const useSkillsStore = create<SkillsState>((set, get) => ({
  agentId: null,
  skills: [],
  loading: false,
  refreshing: false,
  error: null,

  load: async (agentId: string) => {
    set({ agentId, loading: true, error: null });
    try {
      const skills = await api.listAgentSkills(agentId);
      if (get().agentId !== agentId) return;
      set({ skills, loading: false });
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
      const skills = await api.refreshAgentSkills(agentId);
      if (get().agentId !== agentId) return;
      set({ skills, refreshing: false });
    } catch (err) {
      if (get().agentId !== agentId) return;
      set({ refreshing: false, error: errorMessage(err) });
    }
  },

  importFolder: async (path: string) => {
    const { agentId } = get();
    if (!agentId) return;
    set({ error: null });
    try {
      const imported = await api.importAgentSkillFolder(agentId, path);
      if (get().agentId !== agentId) return;
      set((state) => ({ skills: mergeSkills(state.skills, imported) }));
    } catch (err) {
      set({ error: errorMessage(err) });
      throw err;
    }
  },

  importFile: async (path: string) => {
    const { agentId } = get();
    if (!agentId) return;
    set({ error: null });
    try {
      const skill = await api.importAgentSkillFile(agentId, path);
      if (get().agentId !== agentId) return;
      set((state) => ({ skills: mergeSkills(state.skills, [skill]) }));
    } catch (err) {
      set({ error: errorMessage(err) });
      throw err;
    }
  },

  remove: async (skillId: string) => {
    const { agentId, skills } = get();
    if (!agentId) return;
    const previous = skills;
    set({ skills: skills.filter((s) => s.id !== skillId), error: null });
    try {
      await api.deleteAgentSkill(agentId, skillId);
    } catch (err) {
      if (get().agentId !== agentId) return;
      set({ skills: previous, error: errorMessage(err) });
    }
  },

  setEnabled: async (skillId: string, enabled: boolean) => {
    const { agentId, skills } = get();
    if (!agentId) return;
    const previous = skills;
    set({
      skills: skills.map((s) => (s.id === skillId ? { ...s, enabled } : s)),
      error: null,
    });
    try {
      const updated = await api.patchAgentSkill(agentId, skillId, { enabled });
      if (get().agentId !== agentId) return;
      set((state) => ({
        skills: state.skills.map((s) => (s.id === skillId ? updated : s)),
      }));
    } catch (err) {
      if (get().agentId !== agentId) return;
      set({ skills: previous, error: errorMessage(err) });
    }
  },

  setAllEnabled: async (enabled: boolean) => {
    const { agentId, skills } = get();
    if (!agentId || skills.length === 0) return;
    const previous = skills;
    set({ skills: skills.map((s) => ({ ...s, enabled })), error: null });
    try {
      await Promise.all(
        skills.map((s) => api.patchAgentSkill(agentId, s.id, { enabled })),
      );
    } catch (err) {
      if (get().agentId !== agentId) return;
      try {
        const refreshed = await api.listAgentSkills(agentId);
        if (get().agentId !== agentId) return;
        set({ skills: refreshed, error: errorMessage(err) });
      } catch {
        set({ skills: previous, error: errorMessage(err) });
      }
      throw err;
    }
  },

  setAutoSync: async (skillId: string, autoSync: boolean) => {
    const { agentId, skills } = get();
    if (!agentId) return;
    const previous = skills;
    set({
      skills: skills.map((s) => (s.id === skillId ? { ...s, auto_sync: autoSync } : s)),
      error: null,
    });
    try {
      const updated = await api.patchAgentSkill(agentId, skillId, { autoSync });
      if (get().agentId !== agentId) return;
      set((state) => ({
        skills: state.skills.map((s) => (s.id === skillId ? updated : s)),
      }));
    } catch (err) {
      if (get().agentId !== agentId) return;
      set({ skills: previous, error: errorMessage(err) });
    }
  },

  reset: () => {
    set({ agentId: null, skills: [], loading: false, refreshing: false, error: null });
  },
}));
