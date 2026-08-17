import { create } from "zustand";
import { Attachment, TranscriptEntry } from "../types/api";
import * as api from "../lib/api";

export interface ActiveToolCall {
  tool: string;
  input?: Record<string, unknown>;
  startedAt: number;
}

interface PhaseChatState {
  taskId: string | null;
  phaseId: string | null;
  messages: TranscriptEntry[];
  messagesLoading: boolean;
  streamingText: string;
  isTyping: boolean;
  activeToolCalls: ActiveToolCall[];
  scrollToBottomTrigger: number;
  coldStarted: Set<string>; // track "task:phase" keys that have been cold-started

  // Actions
  selectPhase: (taskId: string, phaseId: string) => Promise<void>;
  sendMessage: (content: string, attachmentIds?: string[], attachments?: Attachment[]) => Promise<void>;
  coldStart: (taskId: string, phaseId: string) => Promise<void>;
  appendStreamingDelta: (text: string) => void;
  finalizeStreamingMessage: (text: string) => void;
  setTyping: (typing: boolean) => void;
  addActiveToolCall: (toolCall: { tool: string; input?: Record<string, unknown> }) => void;
  removeActiveToolCall: () => void;
  reset: () => void;
}

export const usePhaseChatStore = create<PhaseChatState>((set, get) => ({
  taskId: null,
  phaseId: null,
  messages: [],
  messagesLoading: false,
  streamingText: "",
  isTyping: false,
  activeToolCalls: [],
  scrollToBottomTrigger: 0,
  coldStarted: new Set(),

  selectPhase: async (taskId: string, phaseId: string) => {
    const { taskId: currentTask, phaseId: currentPhase } = get();
    if (taskId === currentTask && phaseId === currentPhase) return;

    set({ taskId, phaseId, messagesLoading: true, messages: [], streamingText: "", isTyping: false, activeToolCalls: [] });
    try {
      const messages = await api.getPhaseMessages(taskId, phaseId);
      set({ messages, messagesLoading: false });
    } catch (err) {
      console.error("Failed to load phase messages:", err);
      set({ messagesLoading: false });
    }
  },

  sendMessage: async (content: string, attachmentIds?: string[], attachments?: Attachment[]) => {
    const { taskId, phaseId } = get();
    if (!taskId || !phaseId) return;

    // Optimistic user message — include attachment metadata so bubbles render immediately
    const userEntry: TranscriptEntry = {
      ts: new Date().toISOString(),
      role: "user",
      content,
      event_type: "message",
      ...(attachments && attachments.length > 0 ? { attachments } : {}),
    };
    set((s) => ({
      messages: [...s.messages, userEntry],
      scrollToBottomTrigger: s.scrollToBottomTrigger + 1,
    }));

    try {
      await api.sendPhaseMessage(taskId, phaseId, content, attachmentIds);
    } catch (err) {
      console.error("Failed to send phase message:", err);
    }
  },

  coldStart: async (taskId: string, phaseId: string) => {
    const key = `${taskId}:${phaseId}`;
    const { coldStarted } = get();
    if (coldStarted.has(key)) return;

    // Mark as cold-started immediately to prevent double-starts
    set((s) => ({ coldStarted: new Set(s.coldStarted).add(key) }));

    try {
      await api.startPhaseAgent(taskId, phaseId);
    } catch (err) {
      console.error("Failed to cold-start phase agent:", err);
    }
  },

  appendStreamingDelta: (text: string) => {
    set((s) => ({
      streamingText: s.streamingText + text,
      isTyping: true,
    }));
  },

  finalizeStreamingMessage: (text: string) => {
    const agentEntry: TranscriptEntry = {
      ts: new Date().toISOString(),
      role: { agent: "phase-agent" },
      content: text,
      event_type: "message",
    };
    set((s) => ({
      messages: [...s.messages, agentEntry],
      streamingText: "",
      isTyping: false,
      activeToolCalls: [],
      scrollToBottomTrigger: s.scrollToBottomTrigger + 1,
    }));
  },

  setTyping: (typing: boolean) => {
    set({ isTyping: typing });
  },

  addActiveToolCall: (toolCall: { tool: string; input?: Record<string, unknown> }) => {
    set((state) => {
      const withTs = { ...toolCall, startedAt: Date.now() };
      // If this call has input, try to update an existing entry for the same tool that lacks input
      if (toolCall.input) {
        for (let i = state.activeToolCalls.length - 1; i >= 0; i--) {
          if (state.activeToolCalls[i].tool === toolCall.tool && !state.activeToolCalls[i].input) {
            const updated = [...state.activeToolCalls];
            updated[i] = { ...withTs, startedAt: state.activeToolCalls[i].startedAt };
            return { activeToolCalls: updated };
          }
        }
      }
      return { activeToolCalls: [...state.activeToolCalls, withTs] };
    });
  },

  removeActiveToolCall: () => {
    set((state) => {
      if (state.activeToolCalls.length > 1) {
        return { activeToolCalls: state.activeToolCalls.slice(1) };
      }
      return { activeToolCalls: [] };
    });
  },

  reset: () => {
    set({
      taskId: null,
      phaseId: null,
      messages: [],
      messagesLoading: false,
      streamingText: "",
      isTyping: false,
      activeToolCalls: [],
      scrollToBottomTrigger: 0,
    });
  },
}));
