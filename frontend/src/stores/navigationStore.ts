import { create } from "zustand";
import type { ViewId } from "../config/navigation";

const STORAGE_KEY = "nav_selectedSubMenuByView";

function loadFromSession(): Partial<Record<ViewId, string>> {
  try {
    const raw = sessionStorage.getItem(STORAGE_KEY);
    return raw ? JSON.parse(raw) : {};
  } catch {
    return {};
  }
}

function saveToSession(data: Partial<Record<ViewId, string>>) {
  try {
    sessionStorage.setItem(STORAGE_KEY, JSON.stringify(data));
  } catch {
    // ignore
  }
}

interface NavigationState {
  selectedSubMenuByView: Partial<Record<ViewId, string>>;
  sidebarOpen: boolean;
  setSelectedSubMenu: (viewId: ViewId, subMenuSlug: string) => void;
  clearSelectedSubMenu: (viewId: ViewId) => void;
  getSelectedSubMenu: (viewId: ViewId) => string | null;
  toggleSidebar: () => void;
  setSidebarOpen: (open: boolean) => void;
}

export const useNavigationStore = create<NavigationState>((set, get) => ({
  selectedSubMenuByView: loadFromSession(),
  sidebarOpen: true,

  setSelectedSubMenu: (viewId, subMenuSlug) =>
    set((state) => {
      const updated = {
        ...state.selectedSubMenuByView,
        [viewId]: subMenuSlug,
      };
      saveToSession(updated);
      return { selectedSubMenuByView: updated };
    }),

  clearSelectedSubMenu: (viewId) =>
    set((state) => {
      if (state.selectedSubMenuByView[viewId] === undefined) return state;
      const updated = { ...state.selectedSubMenuByView };
      delete updated[viewId];
      saveToSession(updated);
      return { selectedSubMenuByView: updated };
    }),

  getSelectedSubMenu: (viewId) => {
    return get().selectedSubMenuByView[viewId] ?? null;
  },

  toggleSidebar: () =>
    set((state) => ({
      sidebarOpen: !state.sidebarOpen,
    })),

  setSidebarOpen: (open) => set({ sidebarOpen: open }),
}));
