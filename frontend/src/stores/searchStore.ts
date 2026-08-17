import { create } from "zustand";
import type { TranscriptEntry } from "../types/api";
import * as api from "../lib/api";

export interface SearchResult {
  agent_id: string;
  agent_name: string;
  entry: TranscriptEntry;
}

export interface SearchNavigateTarget {
  agentId: string;
  ts: string;
  content: string;
  role: string | { agent: string } | { task_id: string };
  agentName: string;
}

export interface PinnedSearchResult {
  ts: string;
  content: string;
  role: string | { agent: string } | { task_id: string };
  agentName: string;
}

interface SearchState {
  query: string;
  results: SearchResult[];
  isSearching: boolean;
  selectedIndex: number | null;
  isOpen: boolean;
  navigateTarget: SearchNavigateTarget | null;
  pinnedSearchResult: PinnedSearchResult | null;

  setQuery: (q: string) => void;
  executeSearch: () => void;
  clearSearch: () => void;
  setSelectedIndex: (i: number | null) => void;
  openSearch: () => void;
  closeSearch: () => void;
  setNavigateTarget: (target: SearchNavigateTarget | null) => void;
  setPinnedSearchResult: (result: PinnedSearchResult | null) => void;
}

let debounceTimer: ReturnType<typeof setTimeout> | null = null;

export const useSearchStore = create<SearchState>((set, get) => ({
  query: "",
  results: [],
  isSearching: false,
  selectedIndex: null,
  isOpen: false,
  navigateTarget: null,
  pinnedSearchResult: null,

  setQuery: (q: string) => {
    set({ query: q });
    if (debounceTimer) clearTimeout(debounceTimer);
    if (!q.trim()) {
      set({ results: [], isSearching: false, selectedIndex: null });
      return;
    }
    set({ isSearching: true });
    debounceTimer = setTimeout(() => {
      get().executeSearch();
    }, 300);
  },

  executeSearch: async () => {
    const { query } = get();
    if (!query.trim()) {
      set({ results: [], isSearching: false });
      return;
    }
    set({ isSearching: true });
    try {
      const response = await api.searchMessages(query);
      // Only update if query hasn't changed during the request
      if (get().query === query) {
        set({ results: response.results, isSearching: false, selectedIndex: null });
      }
    } catch {
      if (get().query === query) {
        set({ results: [], isSearching: false });
      }
    }
  },

  clearSearch: () => {
    if (debounceTimer) clearTimeout(debounceTimer);
    set({ query: "", results: [], isSearching: false, selectedIndex: null, isOpen: false });
  },

  setSelectedIndex: (i: number | null) => {
    set({ selectedIndex: i });
  },

  openSearch: () => {
    set({ isOpen: true });
  },

  closeSearch: () => {
    set({ isOpen: false });
  },

  setNavigateTarget: (target: SearchNavigateTarget | null) => {
    set({ navigateTarget: target });
  },

  setPinnedSearchResult: (result: PinnedSearchResult | null) => {
    set({ pinnedSearchResult: result });
  },
}));
