import { create } from "zustand";
import { PaginationCursor, Project, ProjectListItem, TranscriptEntry } from "../types/api";
import type { ListTasklistsResponse } from "../types/api";
import * as api from "../lib/api";
import { useChatStore } from "./chatStore";

interface ProjectCacheEntry {
  allMessages: TranscriptEntry[];
  displayCount: number;
  lastAccessed: number;
  cursor: PaginationCursor | null;
  project?: Project | null;
}

const PAGE_SIZE = 50;
const MAX_CACHED_PROJECTS = 10;

interface ProjectState {
  projects: ProjectListItem[];
  projectsLoading: boolean;

  activeProjectId: string | null;
  activeProject: Project | null;

  messages: TranscriptEntry[];
  allMessages: TranscriptEntry[];
  messagesLoading: boolean;
  loadingMore: boolean;
  displayCount: number;
  hasMoreMessages: boolean;
  cursor: PaginationCursor | null;

  messageCache: Map<string, ProjectCacheEntry>;

  streamingText: string;
  isTyping: boolean;
  isLoading: boolean;
  scrollToBottomTrigger: number;

  projectLoadError: string | null;

  activeProjectTasklists: ListTasklistsResponse | null;

  fetchProjects: () => Promise<void>;
  addProjectSnapshot: (project: ProjectListItem) => void;
  selectProject: (projectId: string) => Promise<void>;
  loadMoreMessages: () => Promise<void>;
  sendMessage: (projectId: string, content: string, attachmentIds?: string[]) => Promise<void>;
  appendStreamingDelta: (text: string) => void;
  finalizeStreamingMessage: (text: string) => void;
  setTyping: (typing: boolean) => void;
  fetchProjectTasklists: (projectId: string) => Promise<void>;
  refreshMessages: (projectId: string) => Promise<void>;
  applyProjectStateChange: (projectId: string, status?: string, name?: string) => void;
  invalidateCache: (projectId: string) => void;
  deleteProject: (projectId: string) => Promise<void>;
  reset: () => void;
}

// Status ranks for transition guard — higher = more advanced state.
const STATUS_RANK: Record<string, number> = {
  interviewing: 0,
  active: 1,
  needs_review: 2,
  completed: 2,
  archived: 3,
};

// Deduplicates finalizeStreamingMessage calls: both text_complete and
// run_ended fire at conclusion, but the expensive refetch should only
// run once per run.
const finalizeTimestamps = new Map<string, number>();
const FINALIZE_DEBOUNCE_MS = 2000;

/** Resets the finalize-debounce state. Intended for test isolation only. */
export function _resetFinalizeTimestamps(): void {
  finalizeTimestamps.clear();
}

function evictLRU(cache: Map<string, ProjectCacheEntry>): void {
  while (cache.size > MAX_CACHED_PROJECTS) {
    let oldestKey: string | null = null;
    let oldestTime = Infinity;
    for (const [key, entry] of cache) {
      if (entry.lastAccessed < oldestTime) {
        oldestTime = entry.lastAccessed;
        oldestKey = key;
      }
    }
    if (oldestKey) cache.delete(oldestKey);
    else break;
  }
}

export const useProjectStore = create<ProjectState>((set, get) => ({
  projects: [],
  projectsLoading: false,
  activeProjectId: null,
  activeProject: null,
  messages: [],
  allMessages: [],
  messagesLoading: false,
  loadingMore: false,
  displayCount: PAGE_SIZE,
  hasMoreMessages: false,
  cursor: null,
  messageCache: new Map<string, ProjectCacheEntry>(),
  streamingText: "",
  isTyping: false,
  isLoading: false,
  scrollToBottomTrigger: 0,
  projectLoadError: null,
  activeProjectTasklists: null,

  fetchProjects: async () => {
    set({ projectsLoading: true });
    try {
      const projects = await api.getProjects();
      set({ projects });
    } finally {
      set({ projectsLoading: false });
    }
  },

  addProjectSnapshot: (project: ProjectListItem) => {
    set((state) => {
      if (state.projects.some((p) => p.id === project.id)) return {};
      return { projects: [project, ...state.projects] };
    });
  },

  selectProject: async (projectId: string) => {
    const cached = get().messageCache.get(projectId);

    if (cached) {
      const displayCount = cached.displayCount;
      const messages = cached.allMessages.slice(-displayCount);
      const cache = new Map(get().messageCache);
      cache.set(projectId, { ...cached, lastAccessed: Date.now() });

      // Don't reset streaming state if a run is currently in progress —
      // the SSE stays open across navigation and may still be delivering tokens.
      const currentlyStreaming = get().isTyping || get().streamingText !== "";
      set({
        activeProjectId: projectId,
        activeProject: cached.project ?? null,
        allMessages: cached.allMessages,
        messages,
        displayCount,
        hasMoreMessages: cached.allMessages.length > displayCount || cached.cursor !== null,
        cursor: cached.cursor,
        messagesLoading: false,
        projectLoadError: null,
        ...(currentlyStreaming ? {} : { streamingText: "", isTyping: false }),
        messageCache: cache,
      });

      // Re-request at least `displayCount` so a background refresh of an
      // already-visited (cached) project doesn't reclamp a previously
      // expanded window back down to the server default.
      Promise.all([api.getProject(projectId), api.getProjectMessages(projectId, undefined, displayCount)])
        .then(([project, response]) => {
          if (get().activeProjectId !== projectId) return;
          const freshMessages = response.messages;
          const freshCursor = response.cursor;
          const currentDisplayCount = get().displayCount;
          const bgCache = new Map(get().messageCache);
          bgCache.set(projectId, {
            allMessages: freshMessages,
            displayCount: currentDisplayCount,
            lastAccessed: Date.now(),
            cursor: freshCursor,
            project,
          });
          set({
            activeProject: project,
            allMessages: freshMessages,
            messages: freshMessages.slice(-currentDisplayCount),
            hasMoreMessages: freshMessages.length > currentDisplayCount || freshCursor !== null,
            cursor: freshCursor,
            messageCache: bgCache,
          });
          const projectKey = `project:${projectId}`;
          if (response.pending_form_id) {
            useChatStore.getState().setPendingAsyncFormId(projectKey, response.pending_form_id);
          } else {
            useChatStore.getState().clearPendingAsyncFormId(projectKey);
          }
        })
        .catch((err) => console.error("[projectStore] background refresh failed:", err));
    } else {
      const snapshot = get().projects.find((p) => p.id === projectId);
      const snapshotProject: Project | null = snapshot
        ? {
            id: snapshot.id,
            name: snapshot.name,
            emoji: snapshot.emoji,
            goal: "",
            spec: null,
            agent_id: snapshot.agent_id,
            working_dir: null,
            attachments: [],
            status: snapshot.status,
            summary: null,
            created_at: snapshot.created_at,
            updated_at: snapshot.updated_at,
          }
        : null;

      set({
        activeProjectId: projectId,
        activeProject: snapshotProject,
        messages: [],
        allMessages: [],
        messagesLoading: true,
        projectLoadError: null,
        streamingText: "",
        isTyping: false,
      });

      try {
        const [project, response] = await Promise.all([
          api.getProject(projectId),
          api.getProjectMessages(projectId),
        ]);
        if (get().activeProjectId !== projectId) return;

        const allMessages = response.messages;
        const responseCursor = response.cursor;
        const displayCount = PAGE_SIZE;
        const messages = allMessages.slice(-displayCount);

        const cache = new Map(get().messageCache);
        cache.set(projectId, {
          allMessages,
          displayCount,
          lastAccessed: Date.now(),
          cursor: responseCursor,
          project,
        });
        evictLRU(cache);

        set({
          activeProject: project,
          allMessages,
          messages,
          displayCount,
          hasMoreMessages: allMessages.length > displayCount || responseCursor !== null,
          cursor: responseCursor,
          projectLoadError: null,
          messageCache: cache,
        });

        const projectKey = `project:${projectId}`;
        if (response.pending_form_id) {
          useChatStore.getState().setPendingAsyncFormId(projectKey, response.pending_form_id);
        } else {
          useChatStore.getState().clearPendingAsyncFormId(projectKey);
        }
      } catch (err) {
        console.error("[projectStore] selectProject failed:", err);
        set({
          projectLoadError: "Failed to load project. Please try again.",
          messages: [],
          allMessages: [],
        });
      } finally {
        set({ messagesLoading: false });
      }
    }
  },

  loadMoreMessages: async () => {
    if (get().loadingMore) return;
    set({ loadingMore: true });
    try {
      const { activeProjectId, allMessages, displayCount, messageCache, cursor } = get();
      const newDisplayCount = displayCount + PAGE_SIZE;

      if (newDisplayCount <= allMessages.length) {
        const messages = allMessages.slice(-newDisplayCount);
        if (activeProjectId) {
          const entry = messageCache.get(activeProjectId);
          if (entry) {
            const cache = new Map(messageCache);
            cache.set(activeProjectId, { ...entry, displayCount: newDisplayCount, lastAccessed: Date.now() });
            set({ displayCount: newDisplayCount, messages, hasMoreMessages: allMessages.length > newDisplayCount || cursor !== null, messageCache: cache });
            return;
          }
        }
        set({ displayCount: newDisplayCount, messages, hasMoreMessages: allMessages.length > newDisplayCount || cursor !== null });
        return;
      }

      if (!cursor || !activeProjectId) {
        set({ hasMoreMessages: false });
        return;
      }

      try {
        const response = await api.getProjectMessages(activeProjectId, cursor, PAGE_SIZE);
        if (get().activeProjectId !== activeProjectId) return;

        const fetched = response.messages;
        const newCursor = response.cursor;
        const updatedAll = [...fetched, ...get().allMessages];
        const updatedDisplayCount = get().displayCount + fetched.length;
        const updatedMessages = updatedAll.slice(-updatedDisplayCount);

        const existingEntry = get().messageCache.get(activeProjectId);
        const cache = new Map(get().messageCache);
        cache.set(activeProjectId, {
          allMessages: updatedAll,
          displayCount: updatedDisplayCount,
          lastAccessed: Date.now(),
          cursor: newCursor,
          project: existingEntry?.project,
        });

        set({
          allMessages: updatedAll,
          messages: updatedMessages,
          displayCount: updatedDisplayCount,
          hasMoreMessages: updatedAll.length > updatedDisplayCount || newCursor !== null,
          cursor: newCursor,
          messageCache: cache,
        });
      } catch (err) {
        console.error("[projectStore] loadMoreMessages failed:", err);
      }
    } finally {
      set({ loadingMore: false });
    }
  },

  sendMessage: async (projectId: string, content: string, attachmentIds?: string[]) => {
    const { allMessages, messages, messageCache } = get();

    const optimisticEntry: TranscriptEntry = {
      ts: new Date().toISOString(),
      role: "user",
      content,
      event_type: "message",
      status: "sending",
    };

    const newAllMessages = [...allMessages, optimisticEntry];
    const newMessages = [...messages, optimisticEntry];
    const entry = messageCache.get(projectId);

    if (entry) {
      const cache = new Map(messageCache);
      cache.set(projectId, { ...entry, allMessages: newAllMessages, displayCount: get().displayCount + 1, lastAccessed: Date.now() });
      set({ allMessages: newAllMessages, messages: newMessages, messageCache: cache, scrollToBottomTrigger: get().scrollToBottomTrigger + 1 });
    } else {
      set({ allMessages: newAllMessages, messages: newMessages, scrollToBottomTrigger: get().scrollToBottomTrigger + 1 });
    }

    try {
      await api.sendProjectMessage(projectId, content, attachmentIds);
      set((state) => {
        const updated = state.messages.map((m) => m === optimisticEntry ? { ...m, status: "sent" as const } : m);
        const updatedAll = state.allMessages.map((m) => m === optimisticEntry ? { ...m, status: "sent" as const } : m);
        const cacheEntry = state.messageCache.get(projectId);
        if (cacheEntry) {
          const cache = new Map(state.messageCache);
          cache.set(projectId, { ...cacheEntry, allMessages: updatedAll, lastAccessed: Date.now() });
          return { messages: updated, allMessages: updatedAll, messageCache: cache };
        }
        return { messages: updated, allMessages: updatedAll };
      });
    } catch (err) {
      console.error("[projectStore] sendMessage failed:", err);
      set((state) => {
        const updated = state.messages.map((m) => m === optimisticEntry ? { ...m, status: "error" as const } : m);
        const updatedAll = state.allMessages.map((m) => m === optimisticEntry ? { ...m, status: "error" as const } : m);
        const cacheEntry = state.messageCache.get(projectId);
        if (cacheEntry) {
          const cache = new Map(state.messageCache);
          cache.set(projectId, { ...cacheEntry, allMessages: updatedAll, lastAccessed: Date.now() });
          return { messages: updated, allMessages: updatedAll, messageCache: cache };
        }
        return { messages: updated, allMessages: updatedAll };
      });
    }
  },

  appendStreamingDelta: (text: string) => {
    set((state) => ({ streamingText: state.streamingText + text }));
  },

  finalizeStreamingMessage: (_text: string) => {
    const { activeProjectId } = get();
    set({ streamingText: "", isTyping: false });

    if (!activeProjectId) return;

    // Dedupe: text_complete and run_ended both fire at conclusion. Only run
    // the expensive refetch once per run (within the debounce window).
    const now = Date.now();
    const lastFinalize = finalizeTimestamps.get(activeProjectId) ?? 0;
    if (now - lastFinalize < FINALIZE_DEBOUNCE_MS) return;
    finalizeTimestamps.set(activeProjectId, now);

    // Request at least as much history as is already loaded — otherwise this
    // refetch (which fires after every completed agent turn) silently
    // reclamps `allMessages` back down to the server's bare default, undoing
    // any earlier "load more" and dropping the transcript's opening messages
    // once the project has grown past one page.
    api.getProjectMessages(activeProjectId, undefined, get().displayCount).then((response) => {
      if (get().activeProjectId !== activeProjectId) return;
      const freshMessages = response.messages;
      const freshCursor = response.cursor;
      const displayCount = get().displayCount;
      const existingEntry = get().messageCache.get(activeProjectId);
      const cache = new Map(get().messageCache);
      cache.set(activeProjectId, {
        allMessages: freshMessages,
        displayCount,
        lastAccessed: Date.now(),
        cursor: freshCursor,
        project: existingEntry?.project,
      });
      set({
        allMessages: freshMessages,
        messages: freshMessages.slice(-displayCount),
        hasMoreMessages: freshMessages.length > displayCount || freshCursor !== null,
        cursor: freshCursor,
        messageCache: cache,
      });
    }).catch((err) => console.error("[projectStore] finalizeStreamingMessage refetch failed:", err));

    get().fetchProjects();
  },

  setTyping: (typing: boolean) => {
    set({ isTyping: typing });
  },

  fetchProjectTasklists: async (projectId: string) => {
    try {
      const tasklists = await api.listProjectTasklists(projectId);
      set({ activeProjectTasklists: tasklists });
    } catch (err) {
      console.error("[projectStore] fetchProjectTasklists failed:", err);
    }
  },

  refreshMessages: async (projectId: string) => {
    if (get().activeProjectId !== projectId) return;
    try {
      // See finalizeStreamingMessage: request the already-loaded window size
      // so this refresh doesn't reclamp back down to the server default.
      const response = await api.getProjectMessages(projectId, undefined, get().displayCount);
      if (get().activeProjectId !== projectId) return;
      const allMessages = response.messages;
      const freshCursor = response.cursor;
      const displayCount = get().displayCount;
      const cache = new Map(get().messageCache);
      cache.set(projectId, {
        allMessages,
        displayCount,
        lastAccessed: Date.now(),
        cursor: freshCursor,
        project: get().activeProject,
      });
      set({
        allMessages,
        messages: allMessages.slice(-displayCount),
        cursor: freshCursor,
        hasMoreMessages: allMessages.length > displayCount || freshCursor !== null,
        messageCache: cache,
      });
      const projectKey = `project:${projectId}`;
      if (response.pending_form_id) {
        useChatStore.getState().setPendingAsyncFormId(projectKey, response.pending_form_id);
      } else {
        useChatStore.getState().clearPendingAsyncFormId(projectKey);
      }
    } catch (err) {
      console.error("[projectStore] refreshMessages failed:", err);
    }
  },

  applyProjectStateChange: (projectId: string, status?: string, name?: string) => {
    // Patch the in-memory project immediately so a status transition (e.g.
    // interviewing → active when the interview produces a tasklist) flips the
    // detail view from the interview chat to the workspace without forcing the
    // user to navigate away and back. Driven by the `project.state_changed`
    // SSE event.
    set((state) => {
      const patch = <T extends { status: string; name: string }>(p: T): T => ({
        ...p,
        ...(status !== undefined ? { status: status as T["status"] } : {}),
        ...(name !== undefined ? { name } : {}),
      });

      const next: Partial<ProjectState> = {};

      if (state.activeProjectId === projectId && state.activeProject) {
        next.activeProject = patch(state.activeProject);
      }

      if (state.projects.some((p) => p.id === projectId)) {
        next.projects = state.projects.map((p) =>
          p.id === projectId ? patch(p) : p,
        );
      }

      const cached = state.messageCache.get(projectId);
      if (cached?.project) {
        const cache = new Map(state.messageCache);
        cache.set(projectId, { ...cached, project: patch(cached.project) });
        next.messageCache = cache;
      }

      return next;
    });

    // Pull the full fresh project in the background so workspace-only fields
    // (goal, spec, working_dir) populated during the transition are present,
    // without disturbing the live message stream.
    api
      .getProject(projectId)
      .then((project) => {
        if (get().activeProjectId !== projectId) return;
        set((state) => {
          if (!state.activeProject) return {};
          // Guard: don't let a stale HTTP response revert the optimistically-set
          // status. The SSE event is the authoritative signal; the getProject
          // response might race against the server writing the new state.
          const currentRank = STATUS_RANK[state.activeProject.status] ?? 0;
          const fetchedRank = STATUS_RANK[project.status] ?? 0;
          const merged =
            fetchedRank >= currentRank
              ? project
              : { ...project, status: state.activeProject.status };
          const cached = state.messageCache.get(projectId);
          const cache = new Map(state.messageCache);
          if (cached) cache.set(projectId, { ...cached, project: merged });
          return { activeProject: merged, messageCache: cache };
        });
      })
      .catch((err) =>
        console.error("[projectStore] applyProjectStateChange refresh failed:", err),
      );
  },

  invalidateCache: (projectId: string) => {
    const cache = new Map(get().messageCache);
    cache.delete(projectId);
    set({ messageCache: cache });
  },

  deleteProject: async (projectId: string) => {
    await api.deleteProject(projectId);
    set((state) => {
      const cache = new Map(state.messageCache);
      cache.delete(projectId);
      const isActive = state.activeProjectId === projectId;
      return {
        projects: state.projects.filter((p) => p.id !== projectId),
        ...(isActive ? { activeProjectId: null, activeProject: null } : {}),
        messageCache: cache,
      };
    });
    get().fetchProjects();
  },

  reset: () => {
    set({
      activeProjectId: null,
      activeProject: null,
      messages: [],
      allMessages: [],
      messagesLoading: false,
      loadingMore: false,
      displayCount: PAGE_SIZE,
      hasMoreMessages: false,
      cursor: null,
      scrollToBottomTrigger: 0,
      streamingText: "",
      isTyping: false,
      isLoading: false,
      projectLoadError: null,
      activeProjectTasklists: null,
    });
  },
}));
