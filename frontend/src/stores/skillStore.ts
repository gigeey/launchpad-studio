import { create } from "zustand";
import { Skill, listAgentSkills } from "../lib/api";

interface SkillState {
  /** Cache: agentId → enabled skills for that agent. */
  skillsByAgent: Record<string, Skill[]>;
  loading: boolean;

  /** Fetch enabled skills for an agent. Results are cached per-session. */
  fetchSkills: (agentId: string) => Promise<void>;

  /** Clear the entire cache. */
  clearCache: () => void;
}

export const useSkillStore = create<SkillState>((set, get) => ({
  skillsByAgent: {},
  loading: false,

  fetchSkills: async (agentId) => {
    // Return cached if available
    if (get().skillsByAgent[agentId]) return;

    set({ loading: true });
    try {
      const skills = await listAgentSkills(agentId);
      // Only enabled skills are ones RunSkill can actually dispatch to —
      // mirrors the backend's `render_studio_skills_block` (registry.all_visible()).
      const enabled = skills.filter((s) => s.enabled);
      set((state) => ({
        skillsByAgent: { ...state.skillsByAgent, [agentId]: enabled },
        loading: false,
      }));
    } catch (err) {
      console.warn("Failed to fetch agent skills:", err);
      // Cache empty array so we don't retry on every popover open
      set((state) => ({
        skillsByAgent: { ...state.skillsByAgent, [agentId]: [] },
        loading: false,
      }));
    }
  },

  clearCache: () => set({ skillsByAgent: {} }),
}));
